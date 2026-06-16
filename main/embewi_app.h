#pragma once
#include <stdbool.h>
#include <stdint.h>

// Interface commune aux apps de workload (rainbow, button, …).
// embewi_app_init()       → appelé au boot, après wifi, avant selfcheck
// embewi_app_selfcheck()  → appelé par run_checks() pendant PENDING_VERIFY
// embewi_app_service_start() → l'app démarre son service sur le port donné
//                              (protocole au choix de l'app : HTTP, HTTPS, WS…)
// embewi_app_service_stop()  → arrête le service (POST /app/port, reconfig)
void embewi_app_init(void);
bool embewi_app_selfcheck(void);
void embewi_app_service_start(uint16_t port);
void embewi_app_service_stop(void);
