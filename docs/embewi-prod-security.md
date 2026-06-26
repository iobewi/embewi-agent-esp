# Embewi — Durcissement sécurité production

> Met en œuvre les exigences du contrat §1 (racine de confiance) **sans toucher
> au flux de dev**. Tout est porté par un profil de build séparé et opt-in.
>
> ⚠️ **Opérations eFuse IRRÉVERSIBLES.** Lire entièrement avant le premier flash
> prod. Une erreur ici brique définitivement la capacité à reflasher/déboguer le
> device. **Tester d'abord sur un device sacrifiable.**

---

## 1. Dev vs Prod — le principe

| | Dev (défaut) | Prod (`sdkconfig.defaults.prod`) |
|---|---|---|
| Secure Boot v2 | ❌ off | ✅ bootloader refuse les images non signées |
| Flash encryption | ❌ off | ✅ flash chiffrée (mode RELEASE) |
| Anti-rollback | ❌ off | ✅ downgrade sécu bloqué (eFuse) |
| Mode download ROM | ouvert | secure DL mode |
| Chiffrement NVS | ❌ off | ✅ schéma flash-enc (partition `nvs_keys`) |
| Vérif cert du Core (sortant) | ❌ skip | ✅ CA embarquée (`CONFIG_EMBEWI_VERIFY_CORE_CERT`) |
| Reflash libre | ✅ | ❌ (par design) |

Le dev reste rapide et reflashable. La sécurité ne s'active que via le profil
prod, jamais par défaut.

> **Statut : profil prod build-validé.** Un build complet a confirmé que tout
> l'empilement compile et signe (bootloader Secure Boot v2 inclus). Deux bugs de
> config ont été corrigés au passage : (1) la table de partitions chevauchait à
> `CONFIG_PARTITION_TABLE_OFFSET=0x10000` → offsets désormais auto-placés
> (`partitions_4mb.csv`) ; (2) le chiffrement NVS sélectionnait le schéma HMAC
> (eFuse key id invalide sur C3) → forcé sur le schéma flash-encryption
> (`CONFIG_NVS_SEC_KEY_PROTECT_USING_FLASH_ENC` + partition `nvs_keys`). **Reste
> à valider sur silicium** : le burn eFuse au premier flash (non testable hors
> hardware).

---

## 2. Pré-requis (une fois par environnement)

### 2a. Clé de signature Secure Boot (SECRET — jamais dans le dépôt)

```bash
idf.py secure-generate-signing-key --version 2 --scheme rsa3072 \
       secure_boot_signing_key.pem
```

- Garder cette clé dans un coffre (Vault, HSM, gestionnaire de secrets CI).
- Sa perte = impossibilité de signer de futurs OTA pour les devices déjà
  verrouillés. Sa fuite = un attaquant peut signer des images acceptées.
- `*.pem` est déjà dans `.gitignore` — vérifier qu'elle n'est jamais commitée.

### 2b. CA du Core (PUBLIQUE — authentifie les flux sortants)

Le device vérifie le serveur du Core (`/heartbeat`, `/logs` WS) contre cette CA.
Fournir le certificat de l'autorité qui signe le cert serveur du Core
(cert-manager interne, CA d'entreprise, etc.) :

```bash
cp <votre-core-ca>.pem main/core_ca.pem
```

- Ce n'est **pas** un secret (cert public), mais il est **spécifique à
  l'environnement** → gardé hors dépôt (`.gitignore` `*.pem`).
- Absent au build prod → le build **échoue clairement** (volontaire : pas
  d'embarquement d'une CA silencieusement fausse).

---

## 3. Build prod

Le profil prod s'**empile** sur `sdkconfig.defaults` (il ne le remplace pas) :

```bash
idf.py -B build-prod \
       -DSDKCONFIG=build-prod/sdkconfig.prod \
       -DSDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.defaults.prod" \
       build
```

`-B build-prod` isole les artefacts, `-DSDKCONFIG=build-prod/sdkconfig.prod`
isole la config générée → le `sdkconfig` du dev n'est **jamais** écrasé. Les deux
builds coexistent ; `build-prod/` est gitignoré.

> Pour un build de **validation sans flasher** (vérifier que la config compile),
> une clé de signature et une `core_ca.pem` jetables suffisent — le build ne
> touche aucun eFuse (le verrouillage n'arrive qu'au flash/premier boot).

---

## 4. Premier flash : la séquence eFuse

Au **premier boot** d'une image Secure Boot + Flash Enc, le bootloader brûle les
clés dans les eFuse et chiffre la flash. Ce boot est **plus long** et **ne doit
pas être interrompu** (coupure d'alim = device potentiellement briqué).

```bash
# Flash initial (le device finalise le verrouillage au boot)
idf.py -B build-prod -p <PORT> flash monitor
```

Après ce boot :
- l'UART ne peut plus lire la flash en clair (mode RELEASE) ;
- seules les images signées avec `secure_boot_signing_key.pem` bootent ;
- les OTA suivants (via le Core) doivent livrer des binaires **signés** — la
  chaîne OTA du contrat (§3) reste identique, le bootloader ajoute juste la
  vérification de signature à l'activation.

> **Build reproductible pour OTA.** Les binaires OTA poussés par le Core doivent
> être signés avec la même clé. Le Core (repo séparé) signe l'artefact avant
> `PUT /ota/write` ; le device vérifie au boot (§1 : « Core verifies for
> efficiency, bootloader verifies for trust »).

---

## 5. Anti-rollback : discipline de version

`CONFIG_BOOTLOADER_APP_SECURE_VERSION` (défaut 0) est gravé/incrémenté dans les
eFuse. Règle opérationnelle :

```text
- Incrémenter secure_version UNIQUEMENT pour un correctif de sécurité qui doit
  interdire le retour à l'image vulnérable.
- Une fois le device booté sur secure_version=N, il REFUSE toute image < N.
  Irréversible. Ne pas incrémenter à la légère (chaque valeur consomme un bit
  eFuse, le nombre est limité).
- Les mises à jour fonctionnelles normales gardent le même secure_version.
```

---

## 6. Checklist de revue avant déploiement terrain

```text
[ ] secure_boot_signing_key.pem en coffre, hors dépôt, sauvegardée
[ ] main/core_ca.pem = CA réelle du Core de l'environnement cible
[ ] build prod testé sur un device SACRIFIABLE d'abord
[ ] OTA signé vérifié de bout en bout (prepare → write → activate → boot)
[ ] rollback bootloader validé sur image prod (couper le self-check exprès)
[ ] secure_version aligné avec la politique de sécurité
[ ] mode download ROM : secure (défaut) ou disabled selon le modèle de menace
```

---

## Voir aussi

- `embewi-contract-v2.md` §1 — modèle de sécurité (le « pourquoi »).
- `sdkconfig.defaults.prod` — les options exactes (le « quoi »).
