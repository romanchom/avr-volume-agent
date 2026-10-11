#include "../src/denon_protocol.h"

#include <assert.h>
#include <math.h>
#include <stdio.h>

static void test_status_lines(void)
{
	struct denon_status status = { .max_code = 98 };
	assert(denon_parse_line("MV44", &status) == DENON_UPDATE_VOLUME);
	assert(status.volume_code == 44.0);
	assert(denon_parse_line("MV445", &status) == DENON_UPDATE_VOLUME);
	assert(status.volume_code == 44.5);
	assert(denon_parse_line("MVMAX 72", &status) == DENON_UPDATE_MAX);
	assert(status.max_code == 72);
	assert(denon_parse_line("MVMAX 715", &status) == DENON_UPDATE_MAX);
	assert(status.max_code == 71.5);
	assert(denon_parse_line("MUOFF", &status) == DENON_UPDATE_MUTE);
	assert(!status.mute);
	assert(denon_parse_line("MUON", &status) == DENON_UPDATE_MUTE);
	assert(status.mute);
	assert((status.seen & (DENON_UPDATE_VOLUME | DENON_UPDATE_MAX |
			DENON_UPDATE_MUTE)) == (DENON_UPDATE_VOLUME | DENON_UPDATE_MAX |
			DENON_UPDATE_MUTE));
}

static void test_ignores_unrelated_or_invalid_lines(void)
{
	struct denon_status status = { .max_code = 72, .volume_code = 44 };
	assert(denon_parse_line("MVMAX 99", &status) == 0);
	assert(denon_parse_line("MV99", &status) == 0);
	assert(denon_parse_line("MV?", &status) == 0);
	assert(denon_parse_line("SISAT/CBL", &status) == 0);
	assert(denon_parse_line("MVMAX 99", &status) == 0);
	assert(status.max_code == 72);
	assert(status.volume_code == 44);
}

static void test_slider_mapping(void)
{
	assert(denon_slider_to_code(0.0f, 72) == 0);
	assert(denon_slider_to_code(1.0f, 72) == 72);
	assert(denon_slider_to_code(0.001f, 72) == 7);
	assert(denon_slider_to_code(0.216f, 72) == 43);
	assert(denon_slider_to_code(0.216f, 0) == 0);
	assert(fabsf(denon_code_to_channel_volume(44, 72) - 0.2286f) < 0.001f);
	assert(denon_code_to_channel_volume(44, 0) == 0.0f);
}

int main(void)
{
	test_status_lines();
	test_ignores_unrelated_or_invalid_lines();
	test_slider_mapping();
	puts("Denon protocol tests passed");
	return 0;
}
