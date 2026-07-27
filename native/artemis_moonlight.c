#include "artemis_moonlight.h"

#include <stdatomic.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

#include "Limelight.h"

#define AML_STAGE_STARTING 0
#define AML_STAGE_COMPLETE 1
#define AML_STAGE_FAILED 2
#define AML_ERR_INACTIVE -2

struct AmlSession {
    AmlCallbacks callbacks;
    SERVER_INFORMATION server;
    STREAM_CONFIGURATION stream;
    CONNECTION_LISTENER_CALLBACKS connection_callbacks;
    DECODER_RENDERER_CALLBACKS video_callbacks;
    AUDIO_RENDERER_CALLBACKS audio_callbacks;
    char* address;
    char* app_version;
    char* gfe_version;
    char* rtsp_session_url;
    uint8_t* video_scratch;
    size_t video_scratch_capacity;
    bool started;
};

static _Atomic(AmlSession*) g_session = NULL;

static AmlSession* active_session(void) {
    return atomic_load_explicit(&g_session, memory_order_acquire);
}

static char* duplicate_optional(const char* value) {
    return value == NULL ? NULL : strdup(value);
}

static void report_stage(int stage, int state, int error) {
    AmlSession* session = active_session();
    if (session != NULL && session->callbacks.stage != NULL) {
        session->callbacks.stage(
            session->callbacks.userdata,
            LiGetStageName(stage),
            state,
            error
        );
    }
}

static void stage_starting(int stage) {
    report_stage(stage, AML_STAGE_STARTING, 0);
}

static void stage_complete(int stage) {
    report_stage(stage, AML_STAGE_COMPLETE, 0);
}

static void stage_failed(int stage, int error) {
    report_stage(stage, AML_STAGE_FAILED, error);
}

static void connection_started(void) {
    AmlSession* session = active_session();
    if (session != NULL && session->callbacks.connected != NULL) {
        session->callbacks.connected(session->callbacks.userdata);
    }
}

static void connection_terminated(int error) {
    AmlSession* session = active_session();
    if (session != NULL && session->callbacks.terminated != NULL) {
        session->callbacks.terminated(session->callbacks.userdata, error);
    }
}

static void connection_status_update(int status) {
    AmlSession* session = active_session();
    if (session != NULL && session->callbacks.connection_status != NULL) {
        session->callbacks.connection_status(session->callbacks.userdata, status);
    }
}

static int video_setup(
    int format,
    int width,
    int height,
    int fps,
    void* context,
    int flags
) {
    (void)context;
    (void)flags;
    AmlSession* session = active_session();
    if (session == NULL || session->callbacks.video_setup == NULL) {
        return -1;
    }
    return session->callbacks.video_setup(
        session->callbacks.userdata,
        format,
        width,
        height,
        fps
    );
}

static int video_submit(PDECODE_UNIT unit) {
    AmlSession* session = active_session();
    if (session == NULL || unit == NULL || unit->fullLength <= 0 ||
        session->callbacks.video_frame == NULL) {
        return DR_NEED_IDR;
    }

    size_t required = (size_t)unit->fullLength;
    if (required > session->video_scratch_capacity) {
        uint8_t* resized = realloc(session->video_scratch, required);
        if (resized == NULL) {
            return DR_NEED_IDR;
        }
        session->video_scratch = resized;
        session->video_scratch_capacity = required;
    }

    size_t offset = 0;
    for (PLENTRY entry = unit->bufferList; entry != NULL; entry = entry->next) {
        if (entry->length <= 0 || offset + (size_t)entry->length > required) {
            return DR_NEED_IDR;
        }
        memcpy(
            session->video_scratch + offset,
            entry->data,
            (size_t)entry->length
        );
        offset += (size_t)entry->length;
    }
    if (offset != required) {
        return DR_NEED_IDR;
    }

    session->callbacks.video_frame(
        session->callbacks.userdata,
        session->video_scratch,
        required,
        unit->frameType,
        unit->presentationTimeUs
    );
    return DR_OK;
}

static int audio_setup(
    int audio_configuration,
    const POPUS_MULTISTREAM_CONFIGURATION opus,
    void* context,
    int flags
) {
    (void)audio_configuration;
    (void)context;
    (void)flags;
    AmlSession* session = active_session();
    if (session == NULL || opus == NULL || session->callbacks.audio_setup == NULL) {
        return -1;
    }
    return session->callbacks.audio_setup(
        session->callbacks.userdata,
        opus->sampleRate,
        opus->channelCount,
        opus->streams,
        opus->coupledStreams,
        opus->samplesPerFrame,
        opus->mapping,
        (size_t)opus->channelCount
    );
}

static void audio_packet(char* data, int length) {
    AmlSession* session = active_session();
    if (session != NULL && length >= 0 && session->callbacks.audio_packet != NULL) {
        session->callbacks.audio_packet(
            session->callbacks.userdata,
            (const uint8_t*)data,
            (size_t)length
        );
    }
}

AmlSession* aml_session_create(
    const AmlStartConfig* config,
    const AmlCallbacks* callbacks
) {
    if (config == NULL || callbacks == NULL || config->address == NULL ||
        config->app_version == NULL) {
        return NULL;
    }

    AmlSession* session = calloc(1, sizeof(*session));
    if (session == NULL) {
        return NULL;
    }
    session->address = duplicate_optional(config->address);
    session->app_version = duplicate_optional(config->app_version);
    session->gfe_version = duplicate_optional(config->gfe_version);
    session->rtsp_session_url = duplicate_optional(config->rtsp_session_url);
    if (session->address == NULL || session->app_version == NULL) {
        aml_session_destroy(session);
        return NULL;
    }
    session->callbacks = *callbacks;

    LiInitializeServerInformation(&session->server);
    session->server.address = session->address;
    session->server.serverInfoAppVersion = session->app_version;
    session->server.serverInfoGfeVersion = session->gfe_version;
    session->server.rtspSessionUrl = session->rtsp_session_url;
    session->server.serverCodecModeSupport = config->server_codec_mode_support;

    LiInitializeStreamConfiguration(&session->stream);
    session->stream.width = config->width;
    session->stream.height = config->height;
    session->stream.fps = config->fps;
    session->stream.bitrate = config->bitrate_kbps;
    session->stream.packetSize = config->packet_size;
    session->stream.streamingRemotely = STREAM_CFG_AUTO;
    session->stream.audioConfiguration = config->audio_configuration;
    session->stream.supportedVideoFormats = VIDEO_FORMAT_H264;
    session->stream.clientRefreshRateX100 = config->client_refresh_rate_x100;
    session->stream.colorSpace = COLORSPACE_REC_709;
    session->stream.colorRange = COLOR_RANGE_LIMITED;
    session->stream.encryptionFlags = ENCFLG_ALL;
    memcpy(
        session->stream.remoteInputAesKey,
        config->remote_input_key,
        sizeof(session->stream.remoteInputAesKey)
    );
    memcpy(
        session->stream.remoteInputAesIv,
        config->remote_input_iv,
        sizeof(session->stream.remoteInputAesIv)
    );

    LiInitializeConnectionCallbacks(&session->connection_callbacks);
    session->connection_callbacks.stageStarting = stage_starting;
    session->connection_callbacks.stageComplete = stage_complete;
    session->connection_callbacks.stageFailed = stage_failed;
    session->connection_callbacks.connectionStarted = connection_started;
    session->connection_callbacks.connectionTerminated = connection_terminated;
    session->connection_callbacks.connectionStatusUpdate = connection_status_update;

    LiInitializeVideoCallbacks(&session->video_callbacks);
    session->video_callbacks.setup = video_setup;
    session->video_callbacks.submitDecodeUnit = video_submit;
    session->video_callbacks.capabilities = CAPABILITY_DIRECT_SUBMIT |
        CAPABILITY_REFERENCE_FRAME_INVALIDATION_AVC;

    LiInitializeAudioCallbacks(&session->audio_callbacks);
    session->audio_callbacks.init = audio_setup;
    session->audio_callbacks.decodeAndPlaySample = audio_packet;
    session->audio_callbacks.capabilities =
        CAPABILITY_DIRECT_SUBMIT |
        CAPABILITY_SUPPORTS_ARBITRARY_AUDIO_DURATION;
    return session;
}

int32_t aml_session_network_stats(
    AmlSession* session,
    AmlNetworkStats* stats
) {
    if (session == NULL || stats == NULL || active_session() != session) {
        return AML_ERR_INACTIVE;
    }

    const RTP_AUDIO_STATS* audio = LiGetRTPAudioStats();
    const RTP_VIDEO_STATS* video = LiGetRTPVideoStats();
    if (audio == NULL || video == NULL) {
        return -1;
    }

    stats->audio_packets = audio->packetCountAudio;
    stats->audio_fec_recovered = audio->packetCountFecRecovered;
    stats->audio_fec_failed = audio->packetCountFecFailed;
    stats->audio_out_of_sequence = audio->packetCountOOS;
    stats->audio_invalid = audio->packetCountInvalid;
    stats->video_packets = video->packetCountVideo;
    stats->video_fec_recovered = video->packetCountFecRecovered;
    stats->video_fec_failed = video->packetCountFecFailed;
    stats->video_out_of_sequence = video->packetCountOOS;
    stats->video_invalid = video->packetCountInvalid;
    return 0;
}

int32_t aml_session_start(AmlSession* session) {
    if (session == NULL) {
        return -1;
    }
    AmlSession* expected = NULL;
    if (!atomic_compare_exchange_strong_explicit(
            &g_session,
            &expected,
            session,
            memory_order_acq_rel,
            memory_order_acquire)) {
        return -3;
    }
    int32_t result = LiStartConnection(
        &session->server,
        &session->stream,
        &session->connection_callbacks,
        &session->video_callbacks,
        &session->audio_callbacks,
        session,
        0,
        session,
        0
    );
    if (result == 0) {
        session->started = true;
    } else {
        expected = session;
        atomic_compare_exchange_strong_explicit(
            &g_session,
            &expected,
            NULL,
            memory_order_acq_rel,
            memory_order_acquire
        );
    }
    return result;
}

void aml_session_interrupt(AmlSession* session) {
    if (session != NULL && active_session() == session) {
        LiInterruptConnection();
    }
}

void aml_session_stop(AmlSession* session) {
    if (session == NULL) {
        return;
    }
    if (session->started && active_session() == session) {
        LiStopConnection();
        session->started = false;
    }
    AmlSession* expected = session;
    atomic_compare_exchange_strong_explicit(
        &g_session,
        &expected,
        NULL,
        memory_order_acq_rel,
        memory_order_acquire
    );
}

void aml_session_destroy(AmlSession* session) {
    if (session == NULL) {
        return;
    }
    aml_session_stop(session);
    free(session->video_scratch);
    free(session->address);
    free(session->app_version);
    free(session->gfe_version);
    free(session->rtsp_session_url);
    free(session);
}

static bool can_send_input(AmlSession* session) {
    return session != NULL && session->started && active_session() == session;
}

int32_t aml_mouse_move(AmlSession* session, int16_t x, int16_t y) {
    return can_send_input(session)
        ? LiSendMouseMoveEvent(x, y)
        : AML_ERR_INACTIVE;
}

int32_t aml_mouse_button(AmlSession* session, uint8_t action, int32_t button) {
    return can_send_input(session)
        ? LiSendMouseButtonEvent((char)action, button)
        : AML_ERR_INACTIVE;
}

int32_t aml_scroll(AmlSession* session, int16_t vertical, int16_t horizontal) {
    if (!can_send_input(session)) {
        return AML_ERR_INACTIVE;
    }
    int32_t vertical_result = vertical == 0
        ? 0
        : LiSendHighResScrollEvent(vertical);
    int32_t horizontal_result = horizontal == 0
        ? 0
        : LiSendHighResHScrollEvent(horizontal);
    return vertical_result != 0 ? vertical_result : horizontal_result;
}

int32_t aml_keyboard(
    AmlSession* session,
    int16_t virtual_key,
    uint8_t action,
    uint8_t modifiers
) {
    return can_send_input(session)
        ? LiSendKeyboardEvent2(
            virtual_key,
            (char)action,
            (char)modifiers,
            0
        )
        : AML_ERR_INACTIVE;
}

int32_t aml_controller_arrival(AmlSession* session) {
    return can_send_input(session)
        ? LiSendControllerArrivalEvent(
            0,
            1,
            LI_CTYPE_XBOX,
            0xFFFF,
            LI_CCAP_ANALOG_TRIGGERS
        )
        : AML_ERR_INACTIVE;
}

int32_t aml_controller_state(
    AmlSession* session,
    int32_t buttons,
    uint8_t left_trigger,
    uint8_t right_trigger,
    int16_t left_x,
    int16_t left_y,
    int16_t right_x,
    int16_t right_y
) {
    return can_send_input(session)
        ? LiSendMultiControllerEvent(
            0,
            1,
            buttons,
            left_trigger,
            right_trigger,
            left_x,
            left_y,
            right_x,
            right_y
        )
        : AML_ERR_INACTIVE;
}

int32_t aml_controller_departure(AmlSession* session) {
    return can_send_input(session)
        ? LiSendMultiControllerEvent(0, 0, 0, 0, 0, 0, 0, 0, 0)
        : AML_ERR_INACTIVE;
}

void aml_request_idr(AmlSession* session) {
    if (can_send_input(session)) {
        LiRequestIdrFrame();
    }
}
