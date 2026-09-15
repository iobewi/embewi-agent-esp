# Embewi Agent — migration Rust

Implémentation **device** du contrat [`embewi`](https://github.com/iobewi/embewi)
(`v1alpha1`), rattaché ici en submodule sous `contract/`.

> Cloner avec le contrat : `git clone --recursive …` (ou `git submodule update --init`).

## État

Ce dépôt repart d'une base vierge pour réécrire l'agent en **Rust**
(l'ancienne implémentation ESP-IDF/C est conservée sur la branche
[`firmware-c`](https://github.com/iobewi/embewi-agent-esp/tree/firmware-c)
comme référence fonctionnelle et point de comparaison).

La spec normative reste **`contract/docs/embewi-contract-v2.md`** — source de
vérité Core ↔ Agent, inchangée par la migration de langage.

## Build

_À définir — toolchain et structure du projet Rust en cours de mise en place._
