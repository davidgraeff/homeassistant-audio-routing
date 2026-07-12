import esphome.codegen as cg
import esphome.config_validation as cv
from esphome.components import esp32, binary_sensor, text_sensor
from esphome.components.esp32.const import VARIANT_ESP32
from esphome.const import CONF_ID
from esphome.core import CORE

CODEOWNERS = ["@david"]
DEPENDENCIES = ["esp32", "network"]
AUTO_LOAD = ["binary_sensor", "text_sensor"]

a2dp_bridge_ns = cg.esphome_ns.namespace("a2dp_bridge")
A2DPBridge = a2dp_bridge_ns.class_("A2DPBridge", cg.Component)

CONF_DEVICE_NAME = "device_name"
CONF_RTP_HOST = "rtp_host"
CONF_RTP_PORT = "rtp_port"
CONF_CONNECTED = "connected"
CONF_PEER_NAME = "peer_name"
CONF_PEER_ADDRESS = "peer_address"
CONF_TRACK_TITLE = "track_title"
CONF_TRACK_ARTIST = "track_artist"

CONFIG_SCHEMA = cv.All(
    cv.Schema(
        {
            cv.GenerateID(): cv.declare_id(A2DPBridge),
            cv.Optional(CONF_DEVICE_NAME, default="HA Audio Bridge"): cv.string_strict,
            cv.Required(CONF_RTP_HOST): cv.string_strict,
            cv.Optional(CONF_RTP_PORT, default=46000): cv.port,
            cv.Optional(CONF_CONNECTED): binary_sensor.binary_sensor_schema(
                device_class="connectivity",
            ),
            cv.Optional(CONF_PEER_NAME): text_sensor.text_sensor_schema(),
            cv.Optional(CONF_PEER_ADDRESS): text_sensor.text_sensor_schema(),
            cv.Optional(CONF_TRACK_TITLE): text_sensor.text_sensor_schema(),
            cv.Optional(CONF_TRACK_ARTIST): text_sensor.text_sensor_schema(),
        }
    ).extend(cv.COMPONENT_SCHEMA),
    esp32.only_on_variant(supported=[VARIANT_ESP32]),
)


def _require_esp_idf_classic_bt(config):
    # Classic Bluetooth (BR/EDR + A2DP/AVRCP) only exists under the esp-idf
    # framework's Bluedroid host stack, and only on the original ESP32
    # variant (checked separately via only_on_variant above) — the Arduino
    # framework lacks the classic-BT profile APIs entirely.
    if CORE.using_arduino:
        raise cv.Invalid(
            "a2dp_bridge requires 'framework: type: esp-idf' — classic "
            "Bluetooth (A2DP sink) is not available under the Arduino "
            "framework."
        )
    return config


FINAL_VALIDATE_SCHEMA = _require_esp_idf_classic_bt


async def to_code(config):
    var = cg.new_Pvariable(config[CONF_ID])
    await cg.register_component(var, config)

    cg.add(var.set_device_name(config[CONF_DEVICE_NAME]))
    cg.add(var.set_rtp_target(config[CONF_RTP_HOST], config[CONF_RTP_PORT]))

    if CONF_CONNECTED in config:
        sens = await binary_sensor.new_binary_sensor(config[CONF_CONNECTED])
        cg.add(var.set_connected_binary_sensor(sens))
    if CONF_PEER_NAME in config:
        sens = await text_sensor.new_text_sensor(config[CONF_PEER_NAME])
        cg.add(var.set_peer_name_sensor(sens))
    if CONF_PEER_ADDRESS in config:
        sens = await text_sensor.new_text_sensor(config[CONF_PEER_ADDRESS])
        cg.add(var.set_peer_address_sensor(sens))
    if CONF_TRACK_TITLE in config:
        sens = await text_sensor.new_text_sensor(config[CONF_TRACK_TITLE])
        cg.add(var.set_track_title_sensor(sens))
    if CONF_TRACK_ARTIST in config:
        sens = await text_sensor.new_text_sensor(config[CONF_TRACK_ARTIST])
        cg.add(var.set_track_artist_sensor(sens))

    # Classic BT (BR/EDR) + A2DP sink + AVRCP controller, Bluedroid host
    # stack. Deliberately does NOT touch anything BLE-related — omitting
    # esp32_ble/esp32_ble_tracker/bluetooth_proxy/esp32_improv_ble from the
    # device's YAML (not just from this component) is what keeps this
    # conflict-free; see PLAN.md Section 5.3 for the investigation behind
    # these specific sdkconfig options.
    esp32.add_idf_sdkconfig_option("CONFIG_BT_ENABLED", True)
    esp32.add_idf_sdkconfig_option("CONFIG_BT_BLUEDROID_ENABLED", True)
    esp32.add_idf_sdkconfig_option("CONFIG_BT_CLASSIC_ENABLED", True)
    # AVRCP has no separate Kconfig toggle — BT_A2DP_ENABLE `select`s
    # BT_AVRCP_ENABLED automatically (confirmed in
    # bt/host/bluedroid/Kconfig.in, which calls it "a dummy option
    # currently"). Sink vs. source is purely an esp_a2d_sink_init() vs.
    # esp_a2d_source_init() API-level choice, not a Kconfig option either
    # — there is no CONFIG_BT_A2DP_SINK_ENABLE/CONFIG_BT_AVRC_INCLUDED in
    # this esp-idf version; both were silently-ignored no-ops here before
    # this comment, found while double-checking the sdkconfig that
    # actually got applied on first real hardware boot.
    esp32.add_idf_sdkconfig_option("CONFIG_BT_A2DP_ENABLE", True)
    esp32.add_idf_sdkconfig_option("CONFIG_BT_SSP_ENABLED", True)
    esp32.add_idf_sdkconfig_option("CONFIG_BT_BLE_ENABLED", False)
    # The controller-mode Kconfig `choice` defaults to BLE Only (first
    # listed option) when nothing else selects a mode — nothing else in
    # ESPHome ever needs classic BT, so this was never touched before.
    # esp_bt_controller_enable() requires the mode passed at enable-time
    # to exactly match the mode the controller was init'd with
    # (BT_CONTROLLER_INIT_CONFIG_DEFAULT()'s .mode field reads this
    # Kconfig choice); leaving it at the BLE-only default while
    # requesting ESP_BT_MODE_CLASSIC_BT at enable time fails with
    # ESP_ERR_INVALID_ARG (258) — caught on first real hardware boot.
    esp32.add_idf_sdkconfig_option("CONFIG_BTDM_CTRL_MODE_BR_EDR_ONLY", True)
    esp32.add_idf_sdkconfig_option("CONFIG_BTDM_CTRL_MODE_BLE_ONLY", False)
    esp32.add_idf_sdkconfig_option("CONFIG_BTDM_CTRL_MODE_BTDM", False)
