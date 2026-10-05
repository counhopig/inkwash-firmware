// Alarm tone, urgent siren and todo beep on the ES8311 codec.
#pragma once

namespace tones {

enum class Tone { AlarmRing, Siren, TodoBeep };

// Creates the audio worker; codec initialization is deferred until playback.
bool Init();

void Start(Tone tone);
void Stop();
// Includes queued playback and codec shutdown.
bool Busy();

}  // namespace tones
