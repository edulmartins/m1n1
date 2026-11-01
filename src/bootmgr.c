/* SPDX-License-Identifier: MIT */

#include "../build/build_cfg.h"

#include "bootmgr.h"
#include "cpufreq.h"
#include "fb.h"
#include "heapblock.h"
#include "kboot.h"
#include "mitigations.h"
#include "smp.h"
#include "string.h"
#include "utils.h"

#include "tinf/tinf.h"

#ifdef CHAINLOADING

#define KERNEL_ALIGN (2 << 20)
#define MAX_NAME     256
#define ARM64_MAGIC  0x644d5241 /* "ARM\x64" */

struct boot_files {
    void *kernel;
    size_t kernel_size;
    void *initrd;
    size_t initrd_size;
    void *devicetree;
    size_t devicetree_size;
    char *options;
};

int rust_boot_entry_load(const char *id, const char *devicetree, bool splash,
                         struct boot_files *files);

/* Called from Rust: the entry's splash, pixels as b, g, r, 0, centred on black. */
void bootmgr_paint_splash(const void *pixels, u32 width, u32 height)
{
    if (!fb.ptr || width > fb.width || height > fb.height) {
        printf("bootmgr: splash %ux%u does not fit the screen\n", width, height);
        return;
    }
    fb_fill(0, 0, fb.width, fb.height, int2rgb(0));
    fb_blit((fb.width - width) / 2, (fb.height - height) / 2, width, height, (void *)pixels, width,
            PIX_FMT_XBGR);
}

static bool copy_name(char *dst, const char *src)
{
    size_t len = strnlen(src, MAX_NAME);
    if (len == MAX_NAME)
        return false;
    memcpy(dst, src, len + 1);
    return true;
}

/* A gzip kernel goes behind the heap with room for its image_size; an Image stays. */
static void *unpack_kernel(void *image, size_t size)
{
    const u8 *p = image;
    if (size < 2 || p[0] != 0x1f || p[1] != 0x8b)
        return image;

    /* Nothing may allocate until the result is reserved below. */
    void *dest = heapblock_alloc_aligned(0, KERNEL_ALIGN);
    unsigned int dest_len = 1 << 30, source_len = size;
    int ret = tinf_gzip_uncompress(dest, &dest_len, image, &source_len);
    if (ret != TINF_OK) {
        printf("bootmgr: kernel decompression failed (%d)\n", ret);
        return NULL;
    }
    struct kernel_header *hdr = dest;
    if (dest_len < sizeof(*hdr) || hdr->magic != ARM64_MAGIC) {
        printf("bootmgr: the kernel is not an arm64 Image\n");
        return NULL;
    }
    size_t need = max((size_t)dest_len, (size_t)hdr->image_size);
    void *reserved = heapblock_alloc_aligned(need, KERNEL_ALIGN);
    if (reserved != dest) {
        printf("bootmgr: kernel space moved (%p, %p)\n", dest, reserved);
        return NULL;
    }
    printf("bootmgr: kernel %zu bytes uncompressed to %u\n", size, dest_len);
    return dest;
}

/*
 * Loads and boots the BLS entry `id` from main storage, which must be up.
 * Storage is shut down only once everything is loaded, so a failure before
 * that leaves it usable for another entry. Returns 0 when the kernel is set
 * up to run after the proxy reply, -1 with the reason on the console.
 */
int bootmgr_start(const char *id_arg, const char *devicetree_arg, u64 flags)
{
    char id[MAX_NAME], devicetree[MAX_NAME];

    /* The arguments live in host memory above m1n1's heap; copy them first. */
    if (!copy_name(id, id_arg) || (devicetree_arg && !copy_name(devicetree, devicetree_arg))) {
        printf("bootmgr: argument too long\n");
        return -1;
    }
    if (flags & ~BOOTMGR_NO_SPLASH) {
        printf("bootmgr: unsupported flags 0x%lx\n", flags);
        return -1;
    }

    /* The kernel, initrd and DT are m1n1's now, past any limit the host set. */
    heapblock_set_limit(NULL);

    struct boot_files files;
    if (rust_boot_entry_load(id, devicetree_arg ? devicetree : NULL, !(flags & BOOTMGR_NO_SPLASH),
                             &files) < 0)
        return -1;
    void *kernel = unpack_kernel(files.kernel, files.kernel_size);
    if (!kernel)
        return -1;

    main_storage_shutdown();

    if (kboot_set_chosen("bootargs", files.options) < 0) {
        printf("bootmgr: could not set the boot arguments; storage is shut down\n");
        return -1;
    }
    if (files.initrd_size)
        kboot_set_initrd(files.initrd, files.initrd_size);

    cpufreq_init();
    smp_start_secondaries();
    mitigations_perform();

    if (kboot_prepare_dt(files.devicetree)) {
        printf("bootmgr: could not prepare the device tree; storage is shut down\n");
        return -1;
    }
    printf("bootmgr: starting %s\n", id);
    return kboot_boot(kernel);
}

#else

int bootmgr_start(const char *id, const char *devicetree, u64 flags)
{
    UNUSED(id);
    UNUSED(devicetree);
    UNUSED(flags);
    printf("bootmgr: not supported in this build\n");
    return -1;
}

#endif
