#include "denon_protocol.h"

#include <math.h>
#include <stdlib.h>
#include <string.h>

uint32_t denon_parse_line(const char *line, struct denon_status *status)
{
	char *end;
	uint32_t changed = 0;

	if (strncmp(line, "MVMAX ", 6) == 0) {
		unsigned long value = strtoul(line + 6, &end, 10);
		double max_code = (double)value;
		size_t digits = (size_t)(end - (line + 6));
		if (end != line + 6 && *end == '\0' && value > 0 && value <= 980 &&
				((digits <= 2 && value <= 98) || digits == 3)) {
			if (digits == 3)
				max_code /= 10.0;
			status->max_code = max_code;
			changed |= DENON_UPDATE_MAX;
		}
	} else if (strncmp(line, "MV", 2) == 0 && line[2] >= '0' &&
			line[2] <= '9') {
		double value = strtod(line + 2, &end);
		size_t digits = (size_t)(end - (line + 2));
		if (end != line + 2 && *end == '\0' && digits == 3 &&
				value >= 0 && value <= 980)
			value /= 10.0;
		if (end != line + 2 && *end == '\0' && value >= 0.0 && value <= 98.0) {
			status->volume_code = value;
			changed |= DENON_UPDATE_VOLUME;
		}
	} else if (strcmp(line, "MUON") == 0) {
		status->mute = true;
		changed |= DENON_UPDATE_MUTE;
	} else if (strcmp(line, "MUOFF") == 0) {
		status->mute = false;
		changed |= DENON_UPDATE_MUTE;
	}
	status->seen |= changed;
	return changed;
}

int denon_slider_to_code(float channel_volume, double max_code)
{
	float position;
	if (max_code == 0)
		return 0;
	if (channel_volume < 0.0f)
		channel_volume = 0.0f;
	if (channel_volume > 1.0f)
		channel_volume = 1.0f;
	position = cbrtf(channel_volume);
	return (int)lround(position * max_code);
}

float denon_code_to_channel_volume(double volume_code, double max_code)
{
	float position;
	if (max_code == 0)
		return 0.0f;
	position = (float)(volume_code / max_code);
	if (position < 0.0f)
		position = 0.0f;
	if (position > 1.0f)
		position = 1.0f;
	return position * position * position;
}
