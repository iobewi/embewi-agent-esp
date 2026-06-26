# Runner pytest-embedded des tests cible Unity.
# Flashe l'app de test (device ou QEMU) et exécute tous les TEST_CASE.
#   pytest test/target/pytest_embewi_target.py --target esp32c3
# QEMU (sans hardware) :
#   pytest test/target/pytest_embewi_target.py --target esp32c3 --embedded-services idf,qemu
import pytest


@pytest.mark.esp32c3
@pytest.mark.generic
def test_embewi_config(dut):
    # Pilote le menu unity_run_menu() : lance chaque cas et vérifie 0 échec.
    dut.run_all_single_board_cases(timeout=60)
