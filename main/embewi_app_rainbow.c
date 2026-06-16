// embewi_app_rainbow.c — WS2812 arc-en-ciel sur GPIO 10 via RMT
//
// Protocole WS2812B, ordre GRB, MSB en premier.
// Timing à 10 MHz (100 ns/tick) :
//   bit 0 → H=4ticks(400ns)  L=8ticks(800ns)
//   bit 1 → H=8ticks(800ns)  L=4ticks(400ns)
// Reset : on attend 1 ms après chaque trame (>> 50 µs requis).

#include <stdint.h>
#include "esp_log.h"
#include "driver/rmt_tx.h"
#include "driver/rmt_encoder.h"
#include "esp_http_server.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "embewi_app.h"

#define WS2812_GPIO      10
#define RMT_RES_HZ       10000000   // 10 MHz
#define RAINBOW_PERIOD_MS 20        // ~5 s par cycle complet
#define BRIGHTNESS        60        // 0-255, doux pour les yeux

static const char *TAG = "embewi.rainbow";
static rmt_channel_handle_t s_chan    = NULL;
static rmt_encoder_handle_t s_encoder = NULL;
static httpd_handle_t       s_app_srv = NULL;

static const rmt_transmit_config_t s_tx_cfg = { .loop_count = 0 };

// Conversion HSV → RGB (saturation fixée à max).
// h : 0-255 (correspond à 0-360°), v : 0-255
static void hsv_to_rgb(uint8_t h, uint8_t v,
                        uint8_t *r, uint8_t *g, uint8_t *b) {
    uint8_t region = h / 43;
    uint8_t rem    = (h - region * 43) * 6;
    uint8_t q = (uint8_t)((v * (255u - ((255u * rem) >> 8))) >> 8);
    uint8_t t = (uint8_t)((v * ((255u * rem) >> 8)) >> 8);
    switch (region % 6) {
        case 0: *r = v; *g = t; *b = 0; break;
        case 1: *r = q; *g = v; *b = 0; break;
        case 2: *r = 0; *g = v; *b = t; break;
        case 3: *r = 0; *g = q; *b = v; break;
        case 4: *r = t; *g = 0; *b = v; break;
        default:*r = v; *g = 0; *b = q; break;
    }
}

static void rainbow_task(void *arg) {
    uint8_t hue = 0;
    while (true) {
        uint8_t r, g, b;
        hsv_to_rgb(hue, BRIGHTNESS, &r, &g, &b);
        uint8_t grb[3] = { g, r, b };   // WS2812 = GRB
        rmt_transmit(s_chan, s_encoder, grb, sizeof(grb), &s_tx_cfg);
        rmt_tx_wait_all_done(s_chan, pdMS_TO_TICKS(10));
        hue++;
        vTaskDelay(pdMS_TO_TICKS(RAINBOW_PERIOD_MS));
    }
}

void embewi_app_init(void) {
    rmt_tx_channel_config_t chan_cfg = {
        .gpio_num         = WS2812_GPIO,
        .clk_src          = RMT_CLK_SRC_DEFAULT,
        .resolution_hz    = RMT_RES_HZ,
        .mem_block_symbols = 64,
        .trans_queue_depth = 4,
    };
    if (rmt_new_tx_channel(&chan_cfg, &s_chan) != ESP_OK) {
        ESP_LOGE(TAG, "rmt_new_tx_channel failed");
        return;
    }

    rmt_bytes_encoder_config_t enc_cfg = {
        .bit0  = { .level0=1, .duration0=4, .level1=0, .duration1=8 },
        .bit1  = { .level0=1, .duration0=8, .level1=0, .duration1=4 },
        .flags.msb_first = 1,
    };
    if (rmt_new_bytes_encoder(&enc_cfg, &s_encoder) != ESP_OK) {
        ESP_LOGE(TAG, "rmt_new_bytes_encoder failed");
        return;
    }

    rmt_enable(s_chan);
    xTaskCreate(rainbow_task, "embewi_rainbow", 2048, NULL, 3, NULL);
    ESP_LOGI(TAG, "WS2812 rainbow démarré GPIO%d", WS2812_GPIO);
}

// Selfcheck : le canal RMT est initialisé → hardware OK
bool embewi_app_selfcheck(void) {
    return s_chan != NULL && s_encoder != NULL;
}

static esp_err_t h_status(httpd_req_t *req) {
    httpd_resp_set_type(req, "application/json");
    httpd_resp_sendstr(req, s_chan ? "{\"running\":true}" : "{\"running\":false}");
    return ESP_OK;
}

void embewi_app_service_start(uint16_t port) {
    httpd_config_t cfg = HTTPD_DEFAULT_CONFIG();
    cfg.server_port = port;
    cfg.ctrl_port   = port + 1;   // évite le conflit ctrl socket avec l'httpd embewi
    if (httpd_start(&s_app_srv, &cfg) != ESP_OK) {
        ESP_LOGE(TAG, "app httpd KO port=%u", (unsigned)port);
        return;
    }
    const httpd_uri_t route = {
        .uri = "/status", .method = HTTP_GET, .handler = h_status
    };
    httpd_register_uri_handler(s_app_srv, &route);
    ESP_LOGI(TAG, "app service up port=%u → GET /status", (unsigned)port);
}

void embewi_app_service_stop(void) {
    if (s_app_srv) { httpd_stop(s_app_srv); s_app_srv = NULL; }
}
