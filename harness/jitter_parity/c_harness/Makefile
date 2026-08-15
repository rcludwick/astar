# Makefile for the jitterbuf C parity harness.
#
# Builds main.c against the vendored Steve Kann jitterbuf from astar.
# No copy of jitterbuf.c lives here — we compile straight from the source
# of truth.

CC      ?= cc
CFLAGS  ?= -O2 -Wall -Wextra -std=c99

VENDOR_SRC := ../../../../astar/vendor/iaxclient/lib/libiax2/src
JB_C      := $(VENDOR_SRC)/jitterbuf.c
JB_H_DIR  := $(VENDOR_SRC)

BIN := harness_c
OBJS := main.o jitterbuf.o

all: $(BIN)

$(BIN): $(OBJS)
	$(CC) $(CFLAGS) -o $@ $(OBJS)

main.o: main.c $(JB_H_DIR)/jitterbuf.h
	$(CC) $(CFLAGS) -I$(JB_H_DIR) -c main.c -o main.o

jitterbuf.o: $(JB_C) $(JB_H_DIR)/jitterbuf.h
	$(CC) $(CFLAGS) -I$(JB_H_DIR) -c $(JB_C) -o jitterbuf.o

clean:
	rm -f $(BIN) $(OBJS)

.PHONY: all clean
