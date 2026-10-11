CC ?= cc
CFLAGS ?= -O2 -Wall -Wextra
PIPEWIRE_CFLAGS := $(shell pkg-config --cflags libpipewire-0.3)
PIPEWIRE_LIBS := $(shell pkg-config --libs libpipewire-0.3)

.PHONY: all clean test integration-test

all: avr-volume-agent

avr-volume-agent: src/avr-volume-agent.c src/denon_protocol.c src/denon_protocol.h
	$(CC) $(CFLAGS) $(PIPEWIRE_CFLAGS) -o $@ src/avr-volume-agent.c \
		src/denon_protocol.c $(PIPEWIRE_LIBS) -lm -pthread

test: tests/test_denon_protocol
	./tests/test_denon_protocol

integration-test: avr-volume-agent
	PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests \
		-p 'test_pipewire_integration.py' -v

tests/test_denon_protocol: tests/test_denon_protocol.c src/denon_protocol.c src/denon_protocol.h
	$(CC) $(CFLAGS) -o $@ tests/test_denon_protocol.c src/denon_protocol.c -lm

clean:
	rm -f avr-volume-agent tests/test_denon_protocol
