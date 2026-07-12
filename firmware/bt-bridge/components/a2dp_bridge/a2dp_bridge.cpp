#include "a2dp_bridge.h"

#ifdef USE_ESP_IDF

#include <algorithm>
#include <cstring>

#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/socket.h>

#include "esp_random.h"
#include "esphome/core/log.h"

namespace esphome {
namespace a2dp_bridge {

static const char *const TAG = "a2dp_bridge";

// ESP-IDF Bluedroid callbacks are plain C function pointers; there is
// exactly one instance of this component per firmware, so a single global
// pointer is the standard trampoline pattern (same one ESPHome's own
// esp32_ble component uses internally for the same reason).
static A2DPBridge *g_instance = nullptr;

static void gap_cb_trampoline(esp_bt_gap_cb_event_t event, esp_bt_gap_cb_param_t *param) {
  if (g_instance != nullptr)
    g_instance->on_gap_event(event, param);
}
static void a2dp_cb_trampoline(esp_a2d_cb_event_t event, esp_a2d_cb_param_t *param) {
  if (g_instance != nullptr)
    g_instance->on_a2dp_event(event, param);
}
static void a2dp_data_cb_trampoline(const uint8_t *buf, uint32_t len) {
  if (g_instance != nullptr)
    g_instance->on_a2dp_data(buf, len);
}
static void avrc_cb_trampoline(esp_avrc_ct_cb_event_t event, esp_avrc_ct_cb_param_t *param) {
  if (g_instance != nullptr)
    g_instance->on_avrc_event(event, param);
}

void A2DPBridge::setup() {
  g_instance = this;
  this->rtp_ssrc_ = esp_random();
  this->setup_rtp_socket_();
  this->start_bt_stack_();
}

void A2DPBridge::start_bt_stack_() {
  esp_err_t err;

  esp_bt_controller_config_t bt_cfg = BT_CONTROLLER_INIT_CONFIG_DEFAULT();
  if ((err = esp_bt_controller_init(&bt_cfg)) != ESP_OK) {
    ESP_LOGE(TAG, "esp_bt_controller_init failed: %d", err);
    return;
  }
  if ((err = esp_bt_controller_enable(ESP_BT_MODE_CLASSIC_BT)) != ESP_OK) {
    ESP_LOGE(TAG, "esp_bt_controller_enable failed: %d", err);
    return;
  }
  if ((err = esp_bluedroid_init()) != ESP_OK) {
    ESP_LOGE(TAG, "esp_bluedroid_init failed: %d", err);
    return;
  }
  if ((err = esp_bluedroid_enable()) != ESP_OK) {
    ESP_LOGE(TAG, "esp_bluedroid_enable failed: %d", err);
    return;
  }

  esp_bt_gap_set_device_name(this->device_name_.c_str());
  esp_bt_gap_set_scan_mode(ESP_BT_CONNECTABLE, ESP_BT_GENERAL_DISCOVERABLE);
  esp_bt_gap_register_callback(gap_cb_trampoline);

  // "NoInputNoOutput" IO capability: pairing (SSP "Just Works") completes
  // without this device needing a display/keypad. Simplest reliable
  // option for a headless bridge box; some phones may still show their
  // own confirmation prompt, which is normal and needs nothing from us.
  esp_bt_io_cap_t iocap = ESP_BT_IO_CAP_NONE;
  esp_bt_gap_set_security_param(ESP_BT_SP_IOCAP_MODE, &iocap, sizeof(iocap));
  // Legacy (pre-SSP) pairing fallback for older devices; ignored entirely
  // for SSP pairing. ESP_BT_PIN_TYPE_VARIABLE means pin_code/pin_code_len
  // are unused here and ESP_BT_GAP_PIN_REQ_EVT fires instead.
  esp_bt_gap_set_pin(ESP_BT_PIN_TYPE_VARIABLE, 0, nullptr);

  // AVRC must be initialized before A2DP — confirmed on real hardware:
  // doing it the other way around still "works" but the BT stack itself
  // logs "AVRC Controller is expected to be initialized in advance of
  // A2DP !!!" at boot.
  esp_avrc_ct_init();
  esp_avrc_ct_register_callback(avrc_cb_trampoline);

  esp_a2d_register_callback(a2dp_cb_trampoline);
  esp_a2d_sink_register_data_callback(a2dp_data_cb_trampoline);
  esp_a2d_sink_init();

  ESP_LOGI(TAG, "Classic BT A2DP sink '%s' ready, discoverable", this->device_name_.c_str());
}

void A2DPBridge::setup_rtp_socket_() {
  this->rtp_socket_ = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
  if (this->rtp_socket_ < 0) {
    ESP_LOGE(TAG, "Failed to create RTP UDP socket");
  }
}

void A2DPBridge::loop() {
  // AVRC metadata is polled here rather than pushed via AVRC's
  // change-notification mechanism (esp_avrc_ct_send_register_notification_cmd),
  // which needs an extra RN-capability negotiation round trip before it can
  // be armed. Polling every few seconds is simpler and more robust across
  // esp-idf versions; revisit only if the fixed delay proves too slow for
  // the HA track_title/track_artist sensors in practice.
  if (!this->avrc_connected_)
    return;

  const uint32_t now = millis();
  if (now - this->last_metadata_poll_ >= 5000) {
    this->last_metadata_poll_ = now;
    this->request_track_metadata_();
  }
}

void A2DPBridge::dump_config() {
  ESP_LOGCONFIG(TAG, "A2DP Bluetooth Bridge:");
  ESP_LOGCONFIG(TAG, "  Device name: %s", this->device_name_.c_str());
  ESP_LOGCONFIG(TAG, "  RTP target: %s:%u", this->rtp_host_.c_str(), this->rtp_port_);
}

void A2DPBridge::on_gap_event(esp_bt_gap_cb_event_t event, esp_bt_gap_cb_param_t *param) {
  switch (event) {
    case ESP_BT_GAP_AUTH_CMPL_EVT: {
      if (param->auth_cmpl.stat != ESP_BT_STATUS_SUCCESS) {
        ESP_LOGW(TAG, "Pairing failed, status=%d", param->auth_cmpl.stat);
        break;
      }
      const uint8_t *bda = param->auth_cmpl.bda;
      char addr[18];
      snprintf(addr, sizeof(addr), "%02X:%02X:%02X:%02X:%02X:%02X", bda[0], bda[1], bda[2], bda[3], bda[4], bda[5]);
      std::string name(reinterpret_cast<const char *>(param->auth_cmpl.device_name));
      std::string address(addr);
      ESP_LOGI(TAG, "Paired with %s (%s)", name.c_str(), address.c_str());
      this->peer_name_ = name;
      this->peer_address_ = address;
      this->defer([this, name, address]() {
        if (this->peer_name_sensor_ != nullptr)
          this->peer_name_sensor_->publish_state(name);
        if (this->peer_address_sensor_ != nullptr)
          this->peer_address_sensor_->publish_state(address);
      });
      break;
    }
    case ESP_BT_GAP_CFM_REQ_EVT:
      ESP_LOGI(TAG, "GAP SSP confirmation request, auto-accepting (numeric=%u)", param->cfm_req.num_val);
      esp_bt_gap_ssp_confirm_reply(param->cfm_req.bda, true);
      break;
    case ESP_BT_GAP_KEY_NOTIF_EVT:
      ESP_LOGI(TAG, "GAP SSP passkey notification: %06u", param->key_notif.passkey);
      break;
    case ESP_BT_GAP_PIN_REQ_EVT: {
      ESP_LOGI(TAG, "GAP legacy PIN request, replying with default 0000");
      esp_bt_pin_code_t pin_code = {'0', '0', '0', '0'};
      esp_bt_gap_pin_reply(param->pin_req.bda, true, 4, pin_code);
      break;
    }
    default:
      break;
  }
}

void A2DPBridge::on_a2dp_event(esp_a2d_cb_event_t event, esp_a2d_cb_param_t *param) {
  switch (event) {
    case ESP_A2D_CONNECTION_STATE_EVT: {
      bool now_connected = param->conn_stat.state == ESP_A2D_CONNECTION_STATE_CONNECTED;
      this->connected_ = now_connected;
      ESP_LOGI(TAG, "A2DP connection state: %d", param->conn_stat.state);
      this->defer([this, now_connected]() {
        if (this->connected_sensor_ != nullptr)
          this->connected_sensor_->publish_state(now_connected);
        if (!now_connected) {
          if (this->peer_name_sensor_ != nullptr)
            this->peer_name_sensor_->publish_state("");
          if (this->peer_address_sensor_ != nullptr)
            this->peer_address_sensor_->publish_state("");
          if (this->track_title_sensor_ != nullptr)
            this->track_title_sensor_->publish_state("");
          if (this->track_artist_sensor_ != nullptr)
            this->track_artist_sensor_->publish_state("");
        }
      });
      break;
    }
    case ESP_A2D_AUDIO_CFG_EVT: {
      if (param->audio_cfg.mcc.type == ESP_A2D_MCT_SBC) {
        // samp_freq is a 4-bit field; exactly one bit is set once
        // negotiation has settled on a single rate (this is the
        // *configured* value, not the capability list). Bit meanings
        // per the A2DP SBC codec spec, needs verification against a
        // real phone in spike 8 — different sources may negotiate
        // differently.
        uint8_t freq_bits = param->audio_cfg.mcc.cie.sbc_info.samp_freq;
        if (freq_bits & 0x8)
          this->sample_rate_ = 16000;
        else if (freq_bits & 0x4)
          this->sample_rate_ = 32000;
        else if (freq_bits & 0x2)
          this->sample_rate_ = 44100;
        else if (freq_bits & 0x1)
          this->sample_rate_ = 48000;
        ESP_LOGI(TAG, "A2DP SBC stream configured, sample rate %u Hz", this->sample_rate_);
      }
      break;
    }
    case ESP_A2D_AUDIO_STATE_EVT:
      ESP_LOGD(TAG, "A2DP audio state: %d", param->audio_stat.state);
      break;
    default:
      break;
  }
}

void A2DPBridge::on_a2dp_data(const uint8_t *buf, uint32_t len) {
  // Runs directly on the Bluedroid audio task at real-time rate — no
  // logging, no defer(), nothing beyond the UDP send itself belongs here.
  this->send_rtp_packet_(buf, len);
}

void A2DPBridge::send_rtp_packet_(const uint8_t *payload, size_t len) {
  if (this->rtp_socket_ < 0 || this->rtp_host_.empty())
    return;

  struct sockaddr_in dest {};
  dest.sin_family = AF_INET;
  dest.sin_port = htons(this->rtp_port_);
  if (inet_pton(AF_INET, this->rtp_host_.c_str(), &dest.sin_addr) != 1)
    return;

  // 16-bit stereo PCM frames only (4 bytes/frame) — the A2DP sink always
  // hands us whole frames, but the fragment boundary must stay
  // frame-aligned too, or the split would shift left/right channels on
  // the receiving end.
  constexpr size_t MAX_PAYLOAD = 1024 - (1024 % 4);
  size_t offset = 0;
  while (offset < len) {
    size_t chunk = std::min(MAX_PAYLOAD, len - offset);

    uint8_t packet[12 + MAX_PAYLOAD];
    packet[0] = 0x80;  // V=2, P=0, X=0, CC=0
    // Dynamic payload type; the PipeWire rtp-source config on the
    // receiving end (Section 5.4c / spike 3b) must agree on this, on the
    // sample format (raw native-endian S16LE as delivered by the A2DP
    // sink, not byte-swapped to RFC 3551 L16's big-endian convention —
    // this is a private point-to-point link, not a generic RTP
    // receiver), and on sample rate/channel count. All of this is a
    // spike-8 hardware-verification item, not settled by code review.
    packet[1] = 96;
    packet[2] = static_cast<uint8_t>(this->rtp_sequence_ >> 8);
    packet[3] = static_cast<uint8_t>(this->rtp_sequence_);
    packet[4] = static_cast<uint8_t>(this->rtp_timestamp_ >> 24);
    packet[5] = static_cast<uint8_t>(this->rtp_timestamp_ >> 16);
    packet[6] = static_cast<uint8_t>(this->rtp_timestamp_ >> 8);
    packet[7] = static_cast<uint8_t>(this->rtp_timestamp_);
    packet[8] = static_cast<uint8_t>(this->rtp_ssrc_ >> 24);
    packet[9] = static_cast<uint8_t>(this->rtp_ssrc_ >> 16);
    packet[10] = static_cast<uint8_t>(this->rtp_ssrc_ >> 8);
    packet[11] = static_cast<uint8_t>(this->rtp_ssrc_);
    memcpy(packet + 12, payload + offset, chunk);

    sendto(this->rtp_socket_, packet, 12 + chunk, 0, reinterpret_cast<struct sockaddr *>(&dest), sizeof(dest));

    this->rtp_sequence_++;
    this->rtp_timestamp_ += chunk / 4;  // stereo 16-bit frames, RTP clock rate == sample_rate_
    offset += chunk;
  }
}

void A2DPBridge::on_avrc_event(esp_avrc_ct_cb_event_t event, esp_avrc_ct_cb_param_t *param) {
  switch (event) {
    case ESP_AVRC_CT_CONNECTION_STATE_EVT:
      this->avrc_connected_ = param->conn_stat.connected;
      if (this->avrc_connected_) {
        this->last_metadata_poll_ = millis();
        this->request_track_metadata_();
      }
      break;
    case ESP_AVRC_CT_METADATA_RSP_EVT: {
      uint8_t attr_id = param->meta_rsp.attr_id;
      std::string text(reinterpret_cast<const char *>(param->meta_rsp.attr_text),
                        std::max(0, param->meta_rsp.attr_length));
      this->defer([this, attr_id, text]() {
        if (attr_id == ESP_AVRC_MD_ATTR_TITLE && this->track_title_sensor_ != nullptr) {
          this->track_title_sensor_->publish_state(text);
        } else if (attr_id == ESP_AVRC_MD_ATTR_ARTIST && this->track_artist_sensor_ != nullptr) {
          this->track_artist_sensor_->publish_state(text);
        }
      });
      break;
    }
    default:
      break;
  }
}

void A2DPBridge::request_track_metadata_() {
  // Transaction label is a 4-bit field (0-15) per the AVRCP spec.
  this->avrc_transaction_label_ = (this->avrc_transaction_label_ + 1) & 0x0F;
  esp_avrc_ct_send_metadata_cmd(this->avrc_transaction_label_, ESP_AVRC_MD_ATTR_TITLE | ESP_AVRC_MD_ATTR_ARTIST);
}

}  // namespace a2dp_bridge
}  // namespace esphome

#endif  // USE_ESP_IDF
