# Tests cible (Unity) — Embewi agent

Tests du code **ESP-couplé** qui ne peut pas tourner sur host (NVS, OTA, capteurs,
FreeRTOS). Complémentaire des tests purs `test/host/` (parsing/décision, `make`).

## Stratégie

On compile le fichier firmware **sous test** directement dans cette app, et on
fournit des **test doubles** pour ses dépendances externes. Exemple actuel :
`embewi_config.c` (couche McuConfigMap NVS) avec un faux `embewi_rt()`.

```
test/target/
├── CMakeLists.txt              projet ESP-IDF
├── main/
│   ├── CMakeLists.txt          compile test_config.c + embewi_config.c (WHOLE_ARCHIVE)
│   └── test_config.c           5 TEST_CASE + double embewi_rt() + setUp/tearDown
└── pytest_embewi_target.py     runner pytest-embedded (device ou QEMU)
```

## Lancer

### Sur device
```bash
idf.py -C test/target set-target esp32c3
idf.py -C test/target -p <PORT> flash monitor
# au menu Unity : taper * (tous) ou [config]
```

### Automatisé (CI) — pytest-embedded
```bash
pip install pytest-embedded-idf pytest-embedded-serial-esp
pytest test/target/pytest_embewi_target.py --target esp32c3
```

### Sans hardware — QEMU
```bash
pytest test/target/pytest_embewi_target.py --target esp32c3 \
       --embedded-services idf,qemu
```

## Couverture actuelle

| Module | Couvert | Test double |
|--------|---------|-------------|
| `embewi_config.c` | ✅ write/read, get_int, effacement, génération, json_nvs | `embewi_rt()` |

## À étendre (prochaines cibles)

- **`embewi_ota.c`** — machine d'état write/finish + idempotence `staged`.
  Plus lourd : dépend de `esp_ota_*` (partitions OTA dans la table) et `embewi_log_emit`.
  Nécessite une table de partitions avec slots ota_0/ota_1 + double de `embewi_log_emit`.
- **`embewi_selfcheck.c`** — `storage_check` (canary NVS) + bascule rollback.
- **Idéal à terme** : extraire le code firmware réutilisable de `main/` vers un
  **composant** `embewi_core`, pour que `main` et `test/target` en dépendent
  proprement (au lieu de référencer les `.c` par chemin relatif).
