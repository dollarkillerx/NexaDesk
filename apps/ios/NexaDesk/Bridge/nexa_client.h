// The part of libnexa_client's C ABI (crates/nexa-client/src/abi.rs) the iOS app
// uses. Every message crossing it is protobuf (crates/nexa-client/proto/nexa_client.proto).
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct NexaClient NexaClient;
typedef struct NexaSession NexaSession;

#define NEXA_OK 0
#define NEXA_E_NOT_PERMITTED (-6)
#define NEXA_E_BUFFER_TOO_SMALL (-7)
/// kind for nexa_session_attach_view: a CAMetalLayer*.
#define NEXA_VIEW_CAMETALLAYER 3u

/// Events arrive on the core's threads as an encoded `Event`.
typedef void (*nexa_event_cb)(void *user, const uint8_t *pb, size_t len);

int32_t nexa_init(const uint8_t *cfg, size_t len, nexa_event_cb cb, void *user, NexaClient **out);
void nexa_shutdown(NexaClient *c);
int32_t nexa_connect(NexaClient *c, const uint8_t *req, size_t len, NexaSession **out);
void nexa_session_close(NexaSession *s);
/// Main thread. `handle` is the CAMetalLayer of the video view.
int32_t nexa_session_attach_view(NexaSession *s, void *handle, uint32_t kind, uint32_t w_px, uint32_t h_px, float scale);
int32_t nexa_session_resize_view(NexaSession *s, uint32_t w_px, uint32_t h_px, float scale);
int32_t nexa_session_detach_view(NexaSession *s);
int32_t nexa_session_set_input_active(NexaSession *s, bool active);
int32_t nexa_session_command(NexaSession *s, const uint8_t *cmd, size_t len);
int32_t nexa_last_error(uint8_t *buf, size_t cap, size_t *written);
