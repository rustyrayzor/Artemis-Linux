#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct AmlSession AmlSession;

typedef struct AmlStartConfig {
    const char* address;
    const char* app_version;
    const char* gfe_version;
    const char* rtsp_session_url;
    int32_t server_codec_mode_support;
    int32_t width;
    int32_t height;
    int32_t fps;
    int32_t bitrate_kbps;
    int32_t packet_size;
    int32_t audio_configuration;
    int32_t client_refresh_rate_x100;
    uint8_t remote_input_key[16];
    uint8_t remote_input_iv[16];
} AmlStartConfig;

typedef struct AmlNetworkStats {
    uint32_t audio_packets;
    uint32_t audio_fec_recovered;
    uint32_t audio_fec_failed;
    uint32_t audio_out_of_sequence;
    uint32_t audio_invalid;
    uint32_t video_packets;
    uint32_t video_fec_recovered;
    uint32_t video_fec_failed;
    uint32_t video_out_of_sequence;
    uint32_t video_invalid;
} AmlNetworkStats;

typedef struct AmlCallbacks {
    void* userdata;
    void (*stage)(void* userdata, const char* name, int32_t state, int32_t error);
    void (*connected)(void* userdata);
    void (*terminated)(void* userdata, int32_t error);
    void (*connection_status)(void* userdata, int32_t status);
    int32_t (*video_setup)(
        void* userdata,
        int32_t format,
        int32_t width,
        int32_t height,
        int32_t fps
    );
    void (*video_frame)(
        void* userdata,
        const uint8_t* data,
        size_t length,
        int32_t frame_type,
        uint64_t presentation_time_us
    );
    int32_t (*audio_setup)(
        void* userdata,
        int32_t sample_rate,
        int32_t channels,
        int32_t streams,
        int32_t coupled_streams,
        int32_t samples_per_frame,
        const uint8_t* mapping,
        size_t mapping_length
    );
    void (*audio_packet)(void* userdata, const uint8_t* data, size_t length);
} AmlCallbacks;

AmlSession* aml_session_create(
    const AmlStartConfig* config,
    const AmlCallbacks* callbacks
);
int32_t aml_session_start(AmlSession* session);
void aml_session_interrupt(AmlSession* session);
void aml_session_stop(AmlSession* session);
void aml_session_destroy(AmlSession* session);
int32_t aml_session_network_stats(
    AmlSession* session,
    AmlNetworkStats* stats
);

int32_t aml_mouse_move(AmlSession* session, int16_t x, int16_t y);
int32_t aml_mouse_button(AmlSession* session, uint8_t action, int32_t button);
int32_t aml_scroll(AmlSession* session, int16_t vertical, int16_t horizontal);
int32_t aml_keyboard(
    AmlSession* session,
    int16_t virtual_key,
    uint8_t action,
    uint8_t modifiers
);
int32_t aml_controller_arrival(AmlSession* session);
int32_t aml_controller_state(
    AmlSession* session,
    int32_t buttons,
    uint8_t left_trigger,
    uint8_t right_trigger,
    int16_t left_x,
    int16_t left_y,
    int16_t right_x,
    int16_t right_y
);
int32_t aml_controller_departure(AmlSession* session);
void aml_request_idr(AmlSession* session);

#ifdef __cplusplus
}
#endif
