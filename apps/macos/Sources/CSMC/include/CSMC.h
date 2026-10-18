#ifndef CSMC_H
#define CSMC_H

// The AppleSMC user client, in C so its 80-byte parameter struct has C layout
// by construction. A Swift struct does not: a nested struct's tail padding is
// laid out differently, and the kernel reads the bytes, not the declaration.
//
// Six calls and nothing else. Every decision (which key, what a value means,
// which error is which) lives in the Swift library TcrSMC on top of this.
//
// Each call returns the IOKit kern_return_t. Calls that reach the SMC also
// report the SMC's own result byte in `smc_result` (0 is success, 0x84 is
// "no such key"), because IOKit can succeed while the SMC refuses.

#include <stdint.h>

/// Opens a connection to the AppleSMC service. On success `*connection` holds
/// the handle; returns kIOReturnNotFound (0xe00002f0) when there is no AppleSMC
/// service at all, which is normal in a virtual machine.
int32_t csmc_open(uint32_t *connection);

int32_t csmc_close(uint32_t connection);

/// One key's type (a four-character code) and size in bytes, without its value.
int32_t csmc_key_info(
    uint32_t connection, uint32_t key, uint32_t *type, uint32_t *size, uint8_t *smc_result);

/// Reads one key: its type (a four-character code), its size as the SMC
/// reports it, and up to 32 bytes of value into `bytes`, which must hold 32.
/// A size past 32 copies 32 and reports the real size, so the caller can
/// refuse it.
int32_t csmc_read_key(
    uint32_t connection, uint32_t key, uint32_t *type, uint32_t *size, uint8_t *bytes,
    uint8_t *smc_result);

/// Writes `size` bytes (at most 32) of type `type` to one key. A non-root
/// caller gets kIOReturnNotPrivileged (0xe00002c1), measured 2026-10-18.
int32_t csmc_write_key(
    uint32_t connection, uint32_t key, uint32_t type, uint32_t size, const uint8_t *bytes,
    uint8_t *smc_result);

/// The name of the key at `index` in the SMC's own key table (0 up to the
/// count in `#KEY`).
int32_t csmc_key_at_index(uint32_t connection, uint32_t index, uint32_t *key, uint8_t *smc_result);

#endif
