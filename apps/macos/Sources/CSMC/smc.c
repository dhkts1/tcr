// The AppleSMC user-client protocol: selector 2 (the kernel's struct method)
// with a one-byte command in `data8`. Written from the protocol as the
// open-source SMC tools document it; the command numbers and struct layout
// match Stats (exelban/stats, MIT, SMC/smc.swift), which is where they were
// checked.

#include "CSMC.h"

#include <IOKit/IOKitLib.h>
#include <string.h>

typedef struct {
    uint8_t major, minor, build, reserved;
    uint16_t release;
} SMCVersion;

typedef struct {
    uint16_t version, length;
    uint32_t cpu, gpu, mem;
} SMCPowerLimit;

typedef struct {
    uint32_t dataSize, dataType;
    uint8_t dataAttributes;
} SMCKeyInfo;

typedef struct {
    uint32_t key;
    SMCVersion version;
    SMCPowerLimit powerLimit;
    SMCKeyInfo keyInfo;
    uint8_t result, status, data8;
    uint32_t data32;
    uint8_t bytes[32];
} SMCParam;

// The size the kernel expects, measured on a Mac16,7 with the probe this file
// grew out of. A compiler that lays the struct out differently fails here,
// at build time, rather than sending the kernel a struct it misreads.
_Static_assert(sizeof(SMCParam) == 80, "AppleSMC parameter struct must be 80 bytes");

enum {
    kSMCUserClientMethod = 2,
    kSMCReadBytes = 5,
    kSMCWriteBytes = 6,
    kSMCReadIndex = 8,
    kSMCReadKeyInfo = 9,
};

static kern_return_t call(uint32_t connection, SMCParam *in, SMCParam *out) {
    size_t outSize = sizeof(SMCParam);
    return IOConnectCallStructMethod(
        connection, kSMCUserClientMethod, in, sizeof(SMCParam), out, &outSize);
}

int32_t csmc_open(uint32_t *connection) {
    io_service_t service =
        IOServiceGetMatchingService(kIOMainPortDefault, IOServiceMatching("AppleSMC"));
    if (!service) return kIOReturnNotFound;
    io_connect_t handle = 0;
    kern_return_t kr = IOServiceOpen(service, mach_task_self(), 0, &handle);
    IOObjectRelease(service);
    if (kr == KERN_SUCCESS) *connection = handle;
    return kr;
}

int32_t csmc_close(uint32_t connection) { return IOServiceClose(connection); }

int32_t csmc_key_info(
    uint32_t connection, uint32_t key, uint32_t *type, uint32_t *size, uint8_t *smc_result) {
    SMCParam in, out;
    memset(&in, 0, sizeof in);
    memset(&out, 0, sizeof out);
    in.key = key;
    in.data8 = kSMCReadKeyInfo;
    kern_return_t kr = call(connection, &in, &out);
    *smc_result = out.result;
    if (kr != KERN_SUCCESS || out.result != 0) return kr;
    *type = out.keyInfo.dataType;
    *size = out.keyInfo.dataSize;
    return kr;
}

int32_t csmc_read_key(
    uint32_t connection, uint32_t key, uint32_t *type, uint32_t *size, uint8_t *bytes,
    uint8_t *smc_result) {
    uint32_t dataSize = 0;
    kern_return_t kr = csmc_key_info(connection, key, type, &dataSize, smc_result);
    if (kr != KERN_SUCCESS || *smc_result != 0) return kr;
    *size = dataSize;

    SMCParam in, out;
    memset(&in, 0, sizeof in);
    in.key = key;
    in.keyInfo.dataSize = dataSize;
    in.data8 = kSMCReadBytes;
    memset(&out, 0, sizeof out);
    kr = call(connection, &in, &out);
    *smc_result = out.result;
    if (kr != KERN_SUCCESS || out.result != 0) return kr;
    memcpy(bytes, out.bytes, dataSize > 32 ? 32 : dataSize);
    return kr;
}

int32_t csmc_write_key(
    uint32_t connection, uint32_t key, uint32_t type, uint32_t size, const uint8_t *bytes,
    uint8_t *smc_result) {
    if (size > 32) return kIOReturnBadArgument;
    SMCParam in, out;
    memset(&in, 0, sizeof in);
    memset(&out, 0, sizeof out);
    in.key = key;
    in.data8 = kSMCWriteBytes;
    in.keyInfo.dataSize = size;
    in.keyInfo.dataType = type;
    memcpy(in.bytes, bytes, size);
    kern_return_t kr = call(connection, &in, &out);
    *smc_result = out.result;
    return kr;
}

int32_t csmc_key_at_index(uint32_t connection, uint32_t index, uint32_t *key, uint8_t *smc_result) {
    SMCParam in, out;
    memset(&in, 0, sizeof in);
    memset(&out, 0, sizeof out);
    in.data8 = kSMCReadIndex;
    in.data32 = index;
    kern_return_t kr = call(connection, &in, &out);
    *smc_result = out.result;
    if (kr == KERN_SUCCESS && out.result == 0) *key = out.key;
    return kr;
}
