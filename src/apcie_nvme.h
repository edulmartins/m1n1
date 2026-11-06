/* SPDX-License-Identifier: MIT */
#ifndef APCIE_NVME_H
#define APCIE_NVME_H

#include "types.h"

bool apcie_nvme_init(void);
void apcie_nvme_shutdown(void);
bool apcie_nvme_read(u64 lba, void *buffer);
bool apcie_nvme_read_blocks(u64 lba, u32 count, void *buffer);
u32 apcie_nvme_max_blocks(void);
void apcie_nvme_release_boot_from_host(void);

#endif
