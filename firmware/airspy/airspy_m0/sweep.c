/*
 * Copyright 2026 Auggie <auggie@auggiesoft.com>
 *
 * Corvus firmware sweep mode, built on the Airspy R2/Mini firmware (based on the HackRF project).
 *
 * SPDX-License-Identifier: MIT
 * This file is MIT-licensed (see LICENSE at the top of the Corvus repository). The firmware it is
 * built into is GPL-2.0-or-later as a whole.
 */

#include <stdint.h>

#include <libopencm3/lpc43xx/m0/nvic.h>
#include <r820t.h>

#include "usb.h"
#include "usb_queue.h"
#include "airspy_conf.h"
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
static uint8_t tune_failed; /* the current step is outside the R82x PLL range: nothing is sent for it */
static volatile uint8_t have_restore; /* restore_hz holds the tuner frequency from before the sweep */
static uint32_t restore_hz;

/* Put the tuner back where it was before the sweep. Runs in the USB IRQ or with USB0 masked, so it
 * can't interleave with the main loop's tuner I2C (retune() masks USB0 around each transaction). */
static void restore_tuner(void)
{
	if (have_restore)
	{
		have_restore = 0;
		r820t_set_freq(&airspy_conf->r820t_conf_rw, restore_hz);
	}
}

void sweep_start(void) /* USB IRQ */
{
	start_req = 1;
}

void sweep_halt(void) /* USB IRQ */
{
	start_req = 0;
	active = 0;
}

void sweep_stop(void) /* USB IRQ */
{
	sweep_halt();
	restore_tuner();
}

int sweep_is_active(void)
{
	return active || start_req;
}

static uint32_t step_freq(uint16_t s)
{
	return sweep_params.f_start_hz + (uint32_t)s * sweep_params.f_step_hz;
}

/* The divider r820t_set_pll() picks for this RF frequency (same search). */
static uint8_t vco_divider(uint32_t rf)
{
	uint32_t lo = rf + airspy_conf->r820t_conf_rw.if_freq;
	uint8_t d;

	for (d = 0; d <= 5; d++)
	{
		if (lo <= (3900000000u >> (d + 1)) && (lo << (d + 1)) >= 1770000000u)
			break;
	}
	return d; /* 6 = out of range */
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
 * other way and starts by re-measuring the end step (sweep_poll skips the retune for it). */
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

/* 1 if every step frequency fits in 32 bits (no wrap to a frequency that was never asked for). */
static int plan_fits(void)
{
	uint32_t n = (uint32_t)sweep_params.n_steps - 1u;
	uint32_t hi = (sweep_params.f_step_hz >> 16) * n; /* n < 2^16, so neither product wraps */
	uint32_t lo = (sweep_params.f_step_hz & 0xffffu) * n;
	uint32_t span;

	if (hi >> 16)
		return 0;
	span = hi << 16;
	if (span + lo < span)
		return 0;
	span += lo;
	return sweep_params.f_start_hz + span >= sweep_params.f_start_hz;
}

static void apply_start(void) /* main loop */
{
	nvic_disable_irq(NVIC_USB0_IRQ);
	if (!start_req)
	{
		nvic_enable_irq(NVIC_USB0_IRQ); /* stopped again before we got here */
		return;
	}
	sweep_params = sweep_params_rx;
	start_req = 0;
	if (!have_restore)
	{
		/* First start since the last stop; a restart keeps the frequency from before the first one. */
		restore_hz = airspy_conf->r820t_conf_rw.freq;
		have_restore = 1;
	}
	if (!plan_fits())
	{
		active = 0;
		restore_tuner();
		nvic_enable_irq(NVIC_USB0_IRQ);
		return;
	}
	active = 1;
	nvic_enable_irq(NVIC_USB0_IRQ);
	if (sweep_params.lock_timeout > SWEEP_LOCK_TIMEOUT_MAX)
		sweep_params.lock_timeout = SWEEP_LOCK_TIMEOUT_MAX;
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
	 * r820t_standby). Mask USB0 for the ~0.35 ms of tuner writes so they can't interleave;
	 * the controller keeps moving data on its own and a pending request is served afterwards.
	 */
	uint16_t polls = 0;
	uint8_t run = 0;
	uint8_t v;
	int rc;

	nvic_disable_irq(NVIC_USB0_IRQ);
	if (!active)
	{
		/* Stopped (and the tuner restored) since sweep_poll looked: don't move it again. */
		nvic_enable_irq(NVIC_USB0_IRQ);
		return;
	}
	rc = r820t_set_freq(&airspy_conf->r820t_conf_rw, freq_hz);
	nvic_enable_irq(NVIC_USB0_IRQ);

	tune_failed = (rc < 0);
	if (tune_failed)
	{
		lock_info = 0;
		cnt_ref = *dma_block_count;
		block = 0;
		retune_pending = 0;
		return;
	}

	/*
	 * Wait for PLL lock (as librtlsdr's r82xx_set_pll checks). Lock time grows with the VCO
	 * frequency change (~5.6 ms/GHz, sequential band autotune): ~0.2 ms for 8 MHz hops above 400 MHz,
	 * ~10 ms when an adjacent step switches VCO divider, ~7 ms for a 1796 -> 28 MHz wrap. Requiring several consecutive locked reads guards against a stale
	 * lock indication right after the writes.
	 */
	while (run < sweep_params.lock_reads)
	{
		if (!active || start_req)
			break; /* stopped or restarted: don't hold the main loop for the rest of the wait */
		if (polls >= sweep_params.lock_timeout)
		{
			polls |= 0x8000;
			break;
		}
		nvic_disable_irq(NVIC_USB0_IRQ);
		v = airspy_r820t_read_single(&airspy_conf->r820t_conf_rw, 2);
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
		nvic_disable_irq(NVIC_USB0_IRQ);
		active = 0; /* packing on: not supported */
		restore_tuner();
		nvic_enable_irq(NVIC_USB0_IRQ);
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

	if (tune_failed)
	{
		/* The tuner can't reach this step, so its data would be mislabelled: send nothing, move on. */
		advance();
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
		if (step_freq(step) == freq_hz)
		{
			/* Same frequency (VCO-order turnaround, or a one-step plan): no retune, and the next block
			 * follows on without a gap, so let it through the post-retune skip below. */
			block = 0;
			cnt_ref = cnt - 1u - sweep_params.skip_blocks;
		} else
			retune();
	}
}
