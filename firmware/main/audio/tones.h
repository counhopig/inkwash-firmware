// Alarm tone, urgent siren and todo beep on the ES8311 codec
// (rust-firmware/src/{audio,audio_task}.rs).
#pragma once

namespace tones {

enum class Tone { AlarmRing, Siren, TodoBeep };

// Brings up I2S and the codec (then powers the codec down until needed) and
// starts the tone task. Returns false when the codec does not answer.
bool Init();

void Start(Tone tone);
void Stop();

}  // namespace tones
