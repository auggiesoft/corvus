/*
 * Copyright 2026 Auggie <auggie@auggiesoft.com>
 *
 * Corvus firmware sweep mode, built on the Airspy R2/Mini firmware (based on the HackRF project).
 *
 * SPDX-License-Identifier: MIT
 * This file is MIT-licensed (see LICENSE at the top of the Corvus repository). The firmware it is
 * built into is GPL-2.0-or-later as a whole.
 *
 * Firmware sweep mode: the M0 retunes the R820T itself and only sends 16 KB blocks
 * captured after the tuner has settled, each tagged with a header and trailer:
 *
 *   header  (first 16 bytes): u16 0xA55A, u16 0x5357, u16 step, u16 block, u32 sweep, u32 freq_hz
 *   trailer (last 16 bytes):  u16 0x5AA5, u16 lock, u16 step, u16 block, u32 dma_block_count, u32 freq_hz
 *     lock = PLL lock-bit polls this step needed (bit 15 set: timed out without lock)
 *
 * A block whose header and trailer disagree (or lack the magic) was overwritten by the
 * ADC DMA before USB finished reading it, and must be discarded by the host.
 * Unpacked 16-bit mode only (packing off).
 */

#ifndef __CORVUS_SWEEP_H__
#define __CORVUS_SWEEP_H__

#include <stdint.h>

typedef struct
{
	uint32_t f_start_hz;   /* RF centre of step 0 */
	uint32_t f_step_hz;    /* centre spacing */
	uint16_t n_steps;      /* steps per sweep (>= 1) */
	uint8_t skip_blocks;   /* extra 16 KB blocks dropped after the first post-lock block */
	uint8_t dwell_blocks;  /* blocks sent per step (>= 1) */
	uint8_t lock_reads;    /* consecutive R820T PLL-locked reads (reg 2 bit 6) required before capture; 0 = don't poll */
	uint8_t flags;         /* SWEEP_FLAG_* (sweep4+; ignored by sweep2/3) */
	uint16_t lock_timeout; /* max lock polls (~0.1 ms each) before capturing anyway */
} sweep_params_t;          /* 16 bytes, sent as the OUT data stage of AIRSPY_SWEEP_START */

/*
 * Visit the grid in VCO-continuous order: group the steps by R820T VCO divider and sweep alternate groups
 * downwards, so each divider switch is a big RF jump with almost no VCO movement (~0.5 ms to lock instead
 * of ~10 ms), and reverse the whole order every pass (no wrap). Tags still carry the grid step index.
 */
#define SWEEP_FLAG_VCO_ORDER (0x01)

extern sweep_params_t sweep_params_rx; /* OUT data stage target; applied by the main loop */

void sweep_start(void);
void sweep_stop(void);
int sweep_is_active(void);
void sweep_poll(void);     /* call from the M0 main loop on every wake-up */

#endif /* __CORVUS_SWEEP_H__ */
