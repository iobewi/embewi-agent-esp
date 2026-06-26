# Tests host des helpers purs Embewi — aucun ESP-IDF requis.
#   make        → compile et exécute
#   make clean
CC      ?= gcc
CFLAGS  ?= -std=c11 -Wall -Wextra -Werror -O1 -I../../main
SRC      = test_parse.c ../../main/embewi_parse.c
BIN      = test_parse

.PHONY: all clean
all: $(BIN)
	./$(BIN)

$(BIN): $(SRC)
	$(CC) $(CFLAGS) $(SRC) -o $(BIN)

clean:
	rm -f $(BIN)
