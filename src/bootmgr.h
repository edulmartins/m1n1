/* SPDX-License-Identifier: MIT */

#ifndef BOOTMGR_H
#define BOOTMGR_H

#include "types.h"

#define BOOTMGR_NO_SPLASH BIT(0)

int bootmgr_start(const char *id, const char *devicetree, u64 flags);

#endif
