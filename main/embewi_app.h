#pragma once
#include <stdbool.h>
#include <stdint.h>

// Interface commune aux apps de workload (rainbow, button, …).
// embewi_app_init()       → appelé au boot, après wifi et embewi_cfg_boot_init(),
//                           avant selfcheck. INTERDIT d'écrire dans le namespace
//                           embewi_cfg ici : le snapshot "active" de GET /config
//                           (§4) est déjà figé — toute écriture NVS serait
//                           invisible dans active mais visible dans nvs, créant
//                           une divergence trompeuse sans POST /config du Core.
// embewi_app_selfcheck()  → appelé par run_checks() pendant PENDING_VERIFY
// embewi_app_service_start() → l'app démarre son service sur le port donné
//                              (protocole au choix de l'app : HTTP, HTTPS, WS…)
// embewi_app_service_stop()  → arrête le service (POST /app/port, reconfig)
void embewi_app_init(void);
bool embewi_app_selfcheck(void);
void embewi_app_service_start(uint16_t port);
void embewi_app_service_stop(void);
