// C shim over Apple's ALACEncoder (C++) for the Rust FFI (#1526).
//
// Mirrors the reference usage in Apple's convert-utility/main.cpp:
// input = packed little-endian signed LPCM, output = ALAC VBR packets of
// kALACDefaultFrameSize (4096) frames, magic cookie fetched after init.

#include <new>
#include <stdint.h>
#include <string.h>

#include "ALACEncoder.h"
#include "ALACAudioTypes.h"

namespace {

AudioFormatDescription input_format(double sample_rate, uint32_t channels, uint32_t bit_depth) {
    AudioFormatDescription f;
    memset(&f, 0, sizeof(f));
    f.mSampleRate = sample_rate;
    f.mFormatID = kALACFormatLinearPCM;
    f.mFormatFlags = kALACFormatFlagIsSignedInteger | kALACFormatFlagIsPacked; // little endian
    f.mBytesPerPacket = f.mBytesPerFrame = (bit_depth >> 3) * channels;
    f.mFramesPerPacket = 1;
    f.mChannelsPerFrame = channels;
    f.mBitsPerChannel = bit_depth;
    return f;
}

AudioFormatDescription output_format(double sample_rate, uint32_t channels, uint32_t bit_depth) {
    AudioFormatDescription f;
    memset(&f, 0, sizeof(f));
    f.mSampleRate = sample_rate;
    f.mFormatID = kALACFormatAppleLossless;
    switch (bit_depth) {
        case 16: f.mFormatFlags = 1; break; // kTestFormatFlag_16BitSourceData
        case 20: f.mFormatFlags = 2; break;
        case 24: f.mFormatFlags = 3; break;
        default: f.mFormatFlags = 4; break; // 32
    }
    f.mFramesPerPacket = kALACDefaultFrameSize;
    f.mChannelsPerFrame = channels;
    // VBR: bytes/bits per packet stay 0 (see Apple's main.cpp)
    return f;
}

} // namespace

extern "C" {

void* tune_alac_encoder_create(double sample_rate, uint32_t channels, uint32_t bit_depth) {
    ALACEncoder* enc = new (std::nothrow) ALACEncoder();
    if (!enc) return nullptr;
    enc->SetFrameSize(kALACDefaultFrameSize);
    if (enc->InitializeEncoder(output_format(sample_rate, channels, bit_depth)) != 0) {
        delete enc;
        return nullptr;
    }
    return enc;
}

// Encodes ONE packet worth of input (up to 4096 frames, packed LE signed).
// Returns 0 on success; *io_bytes carries input size in, output size out.
int32_t tune_alac_encode_packet(void* handle, double sample_rate, uint32_t channels,
                                uint32_t bit_depth, const uint8_t* input, uint8_t* output,
                                int32_t* io_bytes) {
    ALACEncoder* enc = static_cast<ALACEncoder*>(handle);
    AudioFormatDescription in = input_format(sample_rate, channels, bit_depth);
    AudioFormatDescription out = output_format(sample_rate, channels, bit_depth);
    // Encode() reads from the buffer without modifying it; the const_cast is
    // imposed by Apple's signature.
    return enc->Encode(in, out, const_cast<unsigned char*>(input), output, io_bytes);
}

uint32_t tune_alac_magic_cookie_size(void* handle, uint32_t channels) {
    return static_cast<ALACEncoder*>(handle)->GetMagicCookieSize(channels);
}

void tune_alac_magic_cookie(void* handle, uint8_t* buf, uint32_t* io_size) {
    static_cast<ALACEncoder*>(handle)->GetMagicCookie(buf, io_size);
}

void tune_alac_encoder_destroy(void* handle) {
    ALACEncoder* enc = static_cast<ALACEncoder*>(handle);
    if (enc) {
        enc->Finish();
        delete enc;
    }
}

} // extern "C"
