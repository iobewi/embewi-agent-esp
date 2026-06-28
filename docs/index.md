# Embewi Agent — Documentation

Firmware **device** (ESP32 / ESP-IDF) du système Embewi. Il implémente le contrat
[`embewi`](https://github.com/iobewi/embewi) (`v1alpha1`) : reçoit les OTA,
s'auto-valide (rollback bootloader borné par watchdog), reçoit sa config runtime
du Core, et émet heartbeat + logs chiffrés horodatés (NTP).

Cette doc couvre ce qui est **propre à l'agent**. La spécification d'interface et
la doc système vivent dans le dépôt de référence : **<https://iobewi.github.io/embewi/>**.

```{toctree}
:maxdepth: 2
:caption: Agent

Sécurité production <embewi-prod-security>
```

## Repères

- **Code & build** : voir le [`README`](https://github.com/iobewi/embewi-agent-esp)
  du dépôt (features, build dev/prod, tests host & cible, partitions, NVS).
- **Contrat Core ↔ Agent** : [site de référence](https://iobewi.github.io/embewi/)
  (`embewi`), rattaché ici en submodule sous `contract/`.
- **Sécurité production** : Secure Boot v2, Flash Encryption, gestion des clés —
  page dédiée ci-dessus.
