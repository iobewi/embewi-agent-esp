// embewi_selfcheck.c — validation locale bornée (contrat §3)
//
// LA séquence qui peut tuer le projet. Invariants :
//  - un PENDING_VERIFY ne reste JAMAIS coincé : deadline esp_timer → reset forcé
//    → le bootloader (CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE) revient seul sur
//      l'image précédente, car mark_valid n'a pas été appelé.
//  - mark_valid n'est appelé QUE si tous les checks passent.
//  - heartbeat ne repasse "running" qu'APRÈS mark_valid (piloté via rt->state).

#include "esp_log.h"
#include "esp_ota_ops.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "embewi_agent.h"
#include "embewi_app.h"

static const char *TAG = "embewi.selfcheck";
static esp_timer_handle_t s_deadline;

// Backstop ultime : si le self-check hang (capteur muet), la deadline tire et
// on reset. Pas de mark_valid → rollback bootloader au reboot.
static void deadline_cb(void *arg) {
    ESP_LOGE(TAG, "deadline pending_verify dépassée → reset (→ rollback)");
    // On ne marque PAS invalid_and_reboot ici : un simple restart suffit, le
    // bootloader voit PENDING_VERIFY non confirmé et bascule. mark_invalid
    // serait plus explicite mais le restart est plus robuste si le hang est
    // dans la stack OTA elle-même.
    esp_restart();
}

// Checks applicatifs. TODO réel : interroger capteurs, FS, boucle de contrôle.
static bool run_checks(void) {
    embewi_runtime_t *rt = embewi_rt();
    rt->chk_app     = embewi_app_selfcheck();
    rt->chk_sensors = true;   // TODO: ping capteurs I2C avec timeout
    rt->chk_storage = true;   // TODO: littlefs montable + writable
    return rt->chk_app && rt->chk_sensors && rt->chk_storage;
}

// Tente le rollback. Si impossible (pas d'image précédente valide), passe FAILED.
// Dans le cas FAILED on ne reboot PAS : le device doit rester joignable pour
// que le Core constate l'état et puisse re-pousser une image saine.
static void do_rollback(embewi_runtime_t *rt, const char *reason) {
    rt->state = EMBEWI_ROLLBACK;
    embewi_log_emit("error", reason);
    ESP_LOGE(TAG, "%s → tentative rollback", reason);

    esp_err_t err = esp_ota_mark_app_invalid_rollback_and_reboot();
    // Si elle retourne, rollback impossible (aucune image valide en fallback).
    rt->state = EMBEWI_FAILED;
    embewi_log_emit("fatal", "rollback impossible — FAILED, en attente Core");
    ESP_LOGE(TAG, "rollback impossible (%s) → FAILED (heartbeat continue)",
             esp_err_to_name(err));
}

static void selfcheck_task(void *arg) {
    embewi_runtime_t *rt = embewi_rt();

    bool ok = run_checks();

    if (ok) {
        esp_err_t err = esp_ota_mark_app_valid_cancel_rollback();
        if (err == ESP_OK) {
            esp_timer_stop(s_deadline);
            rt->state = EMBEWI_RUNNING;
            rt->ota_validated = true;   // ← seul endroit qui le passe à true
            // Promeut le staged en déploiement courant + persiste la taille binaire.
            // Le digest sera recalculé depuis la flash à chaque boot (plus secure).
            embewi_staged_t staged;
            if (embewi_staged_load(&staged) == ESP_OK) {
                strlcpy(rt->deployment_id, staged.deployment_id,
                        sizeof(rt->deployment_id));
                embewi_fw_size_save(staged.size);
            }
            embewi_staged_clear();
            embewi_log_emit("info", "ota validated, mark_valid done");
            ESP_LOGI(TAG, "self-check OK → RUNNING (ota_validated=true)");
        } else {
            do_rollback(rt, "mark_valid failed, rolling back");
        }
    } else {
        do_rollback(rt, "self-check failed, rolling back");
    }
    vTaskDelete(NULL);
}

void embewi_selfcheck_start(void) {
    const esp_timer_create_args_t targs = {
        .callback = &deadline_cb,
        .name = "embewi_pv_deadline",
    };
    ESP_ERROR_CHECK(esp_timer_create(&targs, &s_deadline));
    ESP_ERROR_CHECK(esp_timer_start_once(
        s_deadline, (uint64_t)EMBEWI_PENDING_DEADLINE_MS * 1000));

    xTaskCreate(selfcheck_task, "embewi_selfchk", 4096, NULL, 5, NULL);
}
