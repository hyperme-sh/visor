# Visor companion packs

Make a cat, dog, rabbit, robot, or original anime character by putting artwork and an optional voice in a folder. A single still image is enough. Add clips for more activities as you make them, and reuse any asset across cues. The Linux desktop companion reads packs at startup.

## Make and install a pack

1. Create `companion-packs/moon-rabbit/` in your Hypermesh configuration directory. The default is `~/.config/hypermesh/`. Visor also supports `$XDG_CONFIG_HOME/hypermesh/` and `$HYPERMESH_CONFIG_DIR`.
2. Put a `400×520` PNG called `rabbit.png` in that folder. Use transparency around the character. Save this as `pack.json` next to the PNG.

   ```json
   {
     "format": 1,
     "name": "Moon Rabbit",
     "sprites": {
       "default": { "frames": ["rabbit.png"] },
       "state:sleeping": null
     }
   }
   ```

3. Add `appearance_pack = "moon-rabbit"` at the root of `companion.toml` in the same configuration directory. Restart the companion. Remove the setting and restart to return to the bundled bug.

To share a pack, share the folder, including its manifest and referenced files. Include a license that permits redistribution and modification, attribution for art and voice, and editable source files where available. Use recordings you have permission to share. To make a variant, copy the folder under a new name, edit its files, and select the new folder in `companion.toml`. Pack folder names and asset filenames start with an ASCII letter or digit and can contain letters, digits, `-`, `_`, and `.`. Assets are regular files directly inside the pack folder.

## Bind artwork and voice to cues

`sprites` and `voice` both accept the following selectors. Visor checks the table from top to bottom for each medium, using the first matching usable entry.

| Selector | Input |
| --- | --- |
| `mode:mcp:editor` | Exact mode as reported by Visor. Use `mode:<value>` for any reported mode, including desktop modes such as `mode:listen`. |
| `mode:mcp:*` | Any MCP mode. |
| `verb:listen` | The desktop session verb. All five verbs are available (`view`, `watch`, `listen`, `mouse`, `type`). |
| `state:sleeping` | The operational state. Supported states are `idle`, `automation`, `paused`, `sleeping`, `disconnected`, and `upgrading`. |
| `mood:curious` | A local `mood` setting in `companion.toml`. |
| `vibe:cozy` | A local `vibe` setting in `companion.toml`. |
| `default` | The pack's general fallback. |

Only `idle` and `automation` use the mode, verb, mood, and vibe selectors. During pause, sleep, disconnection, or an update, Visor uses the corresponding state cue and then `default`. The native UI displays actual status with custom artwork and handles pause and Continue. Mood and vibe are optional case-sensitive tags such as `mood = "curious"` and `vibe = "cozy"`. They come from the local settings. Changing them takes effect when the companion restarts.

A sprite clip contains an ordered `frames` array with 1–16 PNGs of `400×520` pixels. Each frame must have visible pixels and fit within 1 MiB. `ms_per_frame` defaults to 150 and accepts 40–10,000. `loop` defaults to `false`, which holds the last frame. For example, `"verb:type": {"frames":["tap-1.png","tap-2.png"],"ms_per_frame":120,"loop":true}` plays two frames while the typing cue is selected.

Omit any entry you have yet to make, set it to `null`, or leave its `frames` list empty. A clip with an invalid or missing frame is skipped as a whole. If nothing in `sprites` matches, Visor renders the bundled artwork for its current state. The optional `cursor` map takes individual `32×32` PNG filenames of up to 32 KiB for `pointer`, `crawl`, `fly`, `reach`, `click1`, `click2`, `type1`, `type2`, or `busy1` through `busy4`. Use the built-in hotspot at pixel `(10, 5)`, measured from the upper-left. Each missing cursor frame falls back to the pack's `pointer` image or the corresponding bundled frame.

## Pick a voice

Add a `voice` map to the same `pack.json` or to a separate voice-only pack. For example, `"voice": { "state:paused": "paused.wav", "state:disconnected": "offline.wav" }` binds two recorded lines. Select it with `voice_pack = "moon-rabbit"` in `companion.toml`. `appearance_pack` and `voice_pack` are independent, so the same rabbit can use several voices, and the same voice can be used with different characters.

Use mono 16-bit PCM WAV at 8–48 kHz, up to three seconds per line. Visor plays a cue when the selected recording changes and interrupts it if the selection changes again. Unmapped cues are silent. Remove `voice_pack` to mute all pack audio. Playback uses the local Linux ALSA `aplay` program at `/usr/bin/aplay`. Pack voices play prerecorded lines. Generated speech and text-to-speech engines are future extensions.

Format 1 uses a JSON manifest of at most 32 KiB with `format`, `name`, and optional `sprites`, `cursor`, and `voice` maps. The limit is 64 bindings per map, 64 distinct sprite filenames, and 16 distinct voice filenames. Visor reads bounded local media files. Packs carry presentation data only. Device actions, prompt content, credentials, and network access stay outside the pack contract.

## Future renderers

The cue names describe what the companion is doing independently of how it appears. A future renderer interface can pass the same state, verb, mode, mood, and vibe into a 3D rig, a projection or hologram renderer, or a robot's display and expression system. Each renderer can declare its supported media and use a still pose, held pose, or silence for missing cues. Keep 2D assets in the same pack when sharing across devices with different renderer support.

The external interface should publish bounded, read-only presentation signals with an explicit disconnected state. Device adapters belong outside packs and use local, owner-selected connections. A robotics adapter should translate cues into its own constrained expressions, hold or stop on loss of the signal, and leave physical motion limits and emergency stop with the device. Format 1 runs in the Linux desktop companion today.
