# Embewi — application de tests cible (Unity, exécutée sur device ou QEMU).
# Build : idf.py -C test/target set-target esp32c3 build
# Voir test/target/README.md.
cmake_minimum_required(VERSION 3.16)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(embewi_target_tests)
