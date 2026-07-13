#pragma once

#include "esphome/core/component.h"
#include "esphome/components/binary_sensor/binary_sensor.h"
#include "esphome/components/text_sensor/text_sensor.h"

#ifdef USE_ESP_IDF

#include <atomic>
#include <cstdint>
#include <string>

#include "esp_a2dp_api.h"
#include "esp_avrc_api.h"
#include "esp_bt.h"
#include "esp_bt_defs.h"
#include "esp_bt_main.h"
#include "esp_gap_bt_api.h"
// Deliberately not including esp_bt_device.h: its esp_bt_dev_set_device_name
// is deprecated in favor of esp_gap_bt_api.h's esp_bt_gap_set_device_name,
// which is what start_bt_stack_() actually calls.

namespace esphome {
namespace a2dp_bridge {

class A2DPBridge : public Component {
 public:
  void setup() override;
  void loop() override;
  void dump_config() override;
  float get_setup_priority() const override { return setup_priority::AFTER_WIFI; }

  void set_device_name(const std::string &name) { this->device_name_ = name; }
  void set_rtp_target(const std::string &host, uint16_t port) {
    this->rtp_host_ = host;
    this->rtp_port_ = port;
  }
  // Individual setters for the HA-facing text/number entities (YAML,
  // `platform: template` with `restore_value: true`) so the RTP target
  // can be repointed from Home Assistant without reflashing. Called from
  // the main loop thread only (HA/API command → entity control()); reads
  // happen from the Bluedroid audio task in send_rtp_packet_() — a config
  // value that changes only on rare user action, not worth a mutex for.
  void set_rtp_host(const std::string &host) { this->rtp_host_ = host; }
  void set_rtp_port(uint16_t port) { this->rtp_port_ = port; }

  void set_connected_binary_sensor(binary_sensor::BinarySensor *s) { this->connected_sensor_ = s; }
  void set_peer_name_sensor(text_sensor::TextSensor *s) { this->peer_name_sensor_ = s; }
  void set_peer_address_sensor(text_sensor::TextSensor *s) { this->peer_address_sensor_ = s; }
  void set_track_title_sensor(text_sensor::TextSensor *s) { this->track_title_sensor_ = s; }
  void set_track_artist_sensor(text_sensor::TextSensor *s) { this->track_artist_sensor_ = s; }

  // Called from Bluedroid task context (BTC task), not the ESPHome main
  // loop task — must not touch sensors/logging that assume the main loop
  // thread directly. Public only because ESP-IDF callbacks are plain C
  // function pointers and the trampolines in the .cpp need to reach these.
  void on_gap_event(esp_bt_gap_cb_event_t event, esp_bt_gap_cb_param_t *param);
  void on_a2dp_event(esp_a2d_cb_event_t event, esp_a2d_cb_param_t *param);
  void on_a2dp_data(const uint8_t *buf, uint32_t len);
  void on_avrc_event(esp_avrc_ct_cb_event_t event, esp_avrc_ct_cb_param_t *param);

 protected:
  void start_bt_stack_();
  void setup_rtp_socket_();
  void send_rtp_packet_(const uint8_t *payload, size_t len);
  void request_track_metadata_();

  std::string device_name_{"HA Audio Bridge"};
  std::string rtp_host_;
  uint16_t rtp_port_{46000};

  binary_sensor::BinarySensor *connected_sensor_{nullptr};
  text_sensor::TextSensor *peer_name_sensor_{nullptr};
  text_sensor::TextSensor *peer_address_sensor_{nullptr};
  text_sensor::TextSensor *track_title_sensor_{nullptr};
  text_sensor::TextSensor *track_artist_sensor_{nullptr};

  int rtp_socket_{-1};
  uint32_t rtp_ssrc_{0};
  uint16_t rtp_sequence_{0};
  uint32_t rtp_timestamp_{0};
  // Negotiated by the SBC codec config event (ESP_A2D_AUDIO_CFG_EVT); the
  // A2DP sink's internal SBC decoder always outputs PCM at this rate, so
  // it's also the RTP clock rate. Defaults to the common case until the
  // real value is known.
  uint32_t sample_rate_{44100};

  bool connected_{false};
  std::string peer_name_;
  std::string peer_address_;

  bool avrc_connected_{false};
  uint8_t avrc_transaction_label_{0};
  uint32_t last_metadata_poll_{0};

  // RTP TX health counters. Written from the Bluedroid audio task in
  // send_rtp_packet_(), read/logged from the main-loop task — hence
  // atomic. The last_* mirrors and last_stats_log_ are touched only by
  // loop() (main task), so they need no synchronization.
  std::atomic<uint32_t> rtp_packets_sent_{0};
  std::atomic<uint32_t> rtp_send_failures_{0};
  std::atomic<int> last_send_errno_{0};
  uint32_t last_packets_sent_{0};
  uint32_t last_send_failures_{0};
  uint32_t last_stats_log_{0};
};

}  // namespace a2dp_bridge
}  // namespace esphome

#endif  // USE_ESP_IDF
