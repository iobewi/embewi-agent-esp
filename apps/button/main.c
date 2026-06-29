// apps/button/main.c — bouton BOOT → compteur exposé sur port app
//
// GPIO lu depuis NVS au boot via la clé "gpio_button" (McuConfigMap §4a).
// Défaut build : CONFIG_EMBEWI_BUTTON_GPIO (GPIO9 sur ESP32-C3/C6/H2, GPIO0 sinon).
// Le compteur est LOCAL à cette app ; il est exposé via GET /sensors
// sur le port applicatif (NVS, défaut 8080), distinct du port embewi.

#include <stdint.h>
#include "esp_log.h"
#include "driver/gpio.h"
#include "esp_http_server.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "embewi_app.h"
#include "embewi_agent.h"

static const char *TAG = "embewi.button";
static int        s_gpio    = CONFIG_EMBEWI_BUTTON_GPIO;
static uint32_t   s_counter = 0;
static httpd_handle_t s_app_srv = NULL;

static void button_task(void *arg) {
    bool was_low = false;
    while (true) {
        bool is_low = (gpio_get_level(s_gpio) == 0);
        if (is_low && !was_low) {
            s_counter++;
            ESP_LOGI(TAG, "press → count=%lu", (unsigned long)s_counter);
        }
        was_low = is_low;
        vTaskDelay(pdMS_TO_TICKS(50));
    }
}

static esp_err_t h_sensors(httpd_req_t *req) {
    char body[48];
    snprintf(body, sizeof(body), "{\"counter\":%lu}", (unsigned long)s_counter);
    httpd_resp_set_type(req, "application/json");
    httpd_resp_sendstr(req, body);
    return ESP_OK;
}

void embewi_app_init(void) {
    s_gpio = embewi_cfg_get_int("gpio_button", CONFIG_EMBEWI_BUTTON_GPIO);
    gpio_config_t io = {
        .pin_bit_mask   = (1ULL << s_gpio),
        .mode           = GPIO_MODE_INPUT,
        .pull_up_en     = GPIO_PULLUP_ENABLE,
        .pull_down_en   = GPIO_PULLDOWN_DISABLE,
        .intr_type      = GPIO_INTR_DISABLE,
    };
    gpio_config(&io);
    xTaskCreate(button_task, "embewi_btn", 2048, NULL, 3, NULL);
    ESP_LOGI(TAG, "bouton BOOT GPIO%d initialisé", s_gpio);
}

bool embewi_app_selfcheck(void) {
    return gpio_get_level(s_gpio) == 1;
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
        .uri = "/sensors", .method = HTTP_GET, .handler = h_sensors
    };
    httpd_register_uri_handler(s_app_srv, &route);
    ESP_LOGI(TAG, "app service up port=%u → GET /sensors", (unsigned)port);
}

void embewi_app_service_stop(void) {
    if (s_app_srv) { httpd_stop(s_app_srv); s_app_srv = NULL; }
}
