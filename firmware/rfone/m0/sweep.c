/*
 * Copyright 2026 Auggie <auggie@auggiesoft.com>
 *
 * Corvus firmware sweep mode, built on the RFOne firmware (based on the HackRF project).
 *
 * SPDX-License-Identifier: MIT
 * This file is MIT-licensed (see LICENSE at the top of the Corvus repository). The firmware it is
 * built into is GPL-2.0-or-later as a whole.
 */

#include <stdint.h>

#include <libopencm3/lpc43xx/m0/nvic.h>
#include <r82x.h>

#include "usb.h"
#include "usb_queue.h"
#include "hydrasdr_conf.h"
#include "sweep.h"

#define SWEEP_BLOCK_BYTES (16384) /* one unpacked DMA half = 8192 raw ADC samples = 409.6 us at 20 MSPS */
#define SWEEP_MAGIC_HDR   (0xA55A)
#define SWEEP_MAGIC_TRL   (0x5AA5)
#define SWEEP_MAGIC_ID    (0x5357)

typedef struct
{
	uint16_t magic;
	uint16_t id;
	uint16_t step;
	uint16_t block;
	uint32_t count;   /* header: sweep number, trailer: DMA block count */
	uint32_t freq_hz;
} sweep_tag_t;

extern usb_endpoint_t usb_endpoint_bulk_in;
extern volatile uint32_t *usb_bulk_buffer_offset;
extern volatile uint32_t *usb_bulk_buffer_length;
extern volatile uint32_t *dma_block_count;
extern uint8_t* const usb_bulk_buffer;

sweep_params_t sweep_params_rx __attribute__ ((aligned(4))); /* written by USB (IRQ) */
static sweep_params_t sweep_params;                          /* the running plan (main loop only) */

/*
 * Start/stop come from USB request handlers in the USB0 IRQ, which can preempt the main loop in the
 * middle of retune() (up to ~7 ms). So the IRQ side only sets flags; the main loop applies a start
 * between retunes (sweep_poll), and a retune in progress can no longer overwrite the new run's state.
 */
static volatile uint8_t active;
static volatile uint8_t start_req;
static uint8_t retune_pending;
static uint16_t step;      /* grid index of the current step */
static uint16_t pos;       /* position in the visiting order */
static int8_t dir;         /* VCO order: direction of the current pass */
#define MAX_RUNS (8)
static uint8_t n_runs;     /* 0 = plain ascending order */
static uint16_t run_start[MAX_RUNS];
static uint16_t run_len[MAX_RUNS];
static uint16_t block;
static uint32_t sweep_no;
static uint32_t freq_hz;
static uint32_t cnt_ref;   /* DMA blocks completed when the last retune locked (or its writes finished) */
static uint32_t last_cnt;
static uint16_t lock_info; /* polls used by the last retune, bit 15 = timed out */

void sweep_start(void) /* USB IRQ */
{
	start_req = 1;
}

void sweep_stop(void) /* USB IRQ or main loop */
{
	start_req = 0;
	active = 0;
}

int sweep_is_active(void)
{
	return active || start_req;
}

static uint32_t step_freq(uint16_t s)
{
	return sweep_params.f_start_hz + (uint32_t)s * sweep_params.f_step_hz;
}

/* The divider r82x_set_pll() picks for this RF frequency (same search). */
static uint8_t vco_divider(uint32_t rf)
{
	uint32_t lo = rf + hydrasdr_conf->r82x_conf_rw.if_freq;
	uint8_t d;

	for (d = 0; d <= 5; d++)
	{
		uint32_t vco = lo << (d + 1);
		if (vco >= 1770000000u && vco <= 3900000000u)
			break;
	}
	return d;
}

/* Split the grid into runs of equal VCO divider (SWEEP_FLAG_VCO_ORDER), else leave n_runs = 0. */
static void plan_order(void)
{
	uint16_t i;
	uint8_t d;
	uint8_t prev = 0xff;

	n_runs = 0;
	if (!(sweep_params.flags & SWEEP_FLAG_VCO_ORDER))
		return;
	for (i = 0; i < sweep_params.n_steps; i++)
	{
		d = vco_divider(step_freq(i));
		if (d != prev)
		{
			if (n_runs == MAX_RUNS)
			{
				n_runs = 0; /* not a monotonic grid: fall back to plain order */
				return;
			}
			run_start[n_runs] = i;
			run_len[n_runs] = 0;
			n_runs++;
			prev = d;
		}
		run_len[n_runs - 1]++;
	}
}

/* Position in the visiting order -> grid step. Odd runs are walked backwards. */
static uint16_t pos_to_step(uint16_t p)
{
	uint8_t r;

	if (n_runs == 0)
		return p;
	for (r = 0; r < n_runs; r++)
	{
		if (p < run_len[r])
			return (r & 1) ? (uint16_t)(run_start[r] + run_len[r] - 1 - p) : (uint16_t)(run_start[r] + p);
		p -= run_len[r];
	}
	return 0;
}

/* Next step. A pass ends after every step was visited once; in VCO order the next pass runs the
 * other way and starts by re-measuring the end step (no retune needed, so it is cheap). */
static void advance(void)
{
	if (n_runs == 0)
	{
		if (++pos >= sweep_params.n_steps)
		{
			pos = 0;
			sweep_no++;
		}
	} else if (dir > 0)
	{
		if (pos + 1u >= sweep_params.n_steps)
		{
			dir = -1;
			sweep_no++;
		} else
			pos++;
	} else
	{
		if (pos == 0)
		{
			dir = 1;
			sweep_no++;
		} else
			pos--;
	}
	step = pos_to_step(pos);
}

static void apply_start(void) /* main loop */
{
	nvic_disable_irq(NVIC_USB0_IRQ);
	if (start_req)
	{
		sweep_params = sweep_params_rx;
		start_req = 0;
		active = 1;
	}
	nvic_enable_irq(NVIC_USB0_IRQ);
	plan_order();
	pos = 0;
	dir = 1;
	step = pos_to_step(0);
	block = 0;
	/* Keeps counting across restarts, so (sweep, step) tags never repeat within a boot. */
	sweep_no++;
	retune_pending = 1;
	last_cnt = *dma_block_count;
}

static void retune(void)
{
	freq_hz = step_freq(step);
	/*
	 * USB request handlers run in the USB0 IRQ and some use I2C1 (e.g. receiver OFF ->
	 * r82x_standby). Mask USB0 for the ~0.35 ms of tuner writes so they can't interleave;
	 * the controller keeps moving data on its own and a pending request is served afterwards.
	 */
	uint16_t polls = 0;
	uint8_t run = 0;
	uint8_t v;

	nvic_disable_irq(NVIC_USB0_IRQ);
	r82x_set_freq(&hydrasdr_conf->r82x_conf_rw, freq_hz);
	nvic_enable_irq(NVIC_USB0_IRQ);

	/*
	 * Wait for PLL lock (as librtlsdr's r82xx_set_pll checks). Lock time grows with the VCO
	 * frequency change (~5.6 ms/GHz, sequential band autotune): ~0.2 ms for 8 MHz hops above 400 MHz,
	 * ~10 ms when an adjacent step switches VCO divider, ~7 ms for a 1796 -> 28 MHz wrap. Requiring several consecutive locked reads guards against a stale
	 * lock indication right after the writes.
	 */
	while (run < sweep_params.lock_reads)
	{
		if (polls >= sweep_params.lock_timeout)
		{
			polls |= 0x8000;
			break;
		}
		nvic_disable_irq(NVIC_USB0_IRQ);
		v = r82x_read_single(&hydrasdr_conf->r82x_conf_rw, 2);
		nvic_enable_irq(NVIC_USB0_IRQ);
		polls++;
		run = (v & 0x40) ? (uint8_t)(run + 1) : 0;
	}
	lock_info = polls;
	cnt_ref = *dma_block_count;
	block = 0;
	retune_pending = 0;
}

static void tag(sweep_tag_t* t, uint16_t magic, uint16_t id, uint32_t count)
{
	t->magic = magic;
	t->id = id;
	t->step = step;
	t->block = block;
	t->count = count;
	t->freq_hz = freq_hz;
}

void sweep_poll(void)
{
	uint32_t cnt;
	uint32_t offset;
	uint8_t* blk;

	if (start_req)
		apply_start();
	if (!active)
		return;

	if (*usb_bulk_buffer_length != SWEEP_BLOCK_BYTES)
	{
		active = 0; /* packing on: not supported */
		return;
	}

	cnt = *dma_block_count;
	if (cnt == last_cnt)
		return;
	last_cnt = cnt;

	if (retune_pending)
	{
		retune();
		return;
	}

	/*
	 * The block that completes as count N started when count was N-1. The block in flight when
	 * the retune finished completes as cnt_ref+1 and started before it, so the first block that
	 * started after the retune completes as cnt_ref+2. skip_blocks more are dropped for PLL settling.
	 */
	if ((uint32_t)(cnt - cnt_ref) < 2u + sweep_params.skip_blocks)
		return;

	offset = *usb_bulk_buffer_offset; /* the half that just completed */
	blk = &usb_bulk_buffer[offset];
	tag((sweep_tag_t*)blk, SWEEP_MAGIC_HDR, SWEEP_MAGIC_ID, sweep_no);
	tag((sweep_tag_t*)(blk + SWEEP_BLOCK_BYTES - sizeof(sweep_tag_t)), SWEEP_MAGIC_TRL, lock_info, cnt);
	usb_transfer_schedule_block(&usb_endpoint_bulk_in, blk, SWEEP_BLOCK_BYTES);

	if (++block >= sweep_params.dwell_blocks)
	{
		advance();
		retune();
	}
}
