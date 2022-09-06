# csjsound-wasapi
WASAPI-EXCLUSIVE DLL for the CleanSine javasound provider https://github.com/pavhofman/csjsound-provider

## Building
```
cargo build
```
## Logging
Logging paramaters are passed from the java provider to native library in native init method params `SimpleMixerProvider.nInit()`, read from java properties. For details see https://github.com/pavhofman/csjsound-provider/blob/main/README.md#native-library-logs. 

Note: The current WASAPI implementation outputs to stdout even for `csjsoundLibLogFile=stderr`.


## Detected Formats
The javasound API requires a list of pre-determined formats supported by the devices. The native library sequentially tries combinations of rates/channels/sample formats/channel masks to find formats supported by the actual device. Tested rates and channels are passed from java to the native library as parameters of the `SimpleMixerProvider.nInit()` native method. The values are either specified by java properties:

```
-DcsjsoundRates=44100,48000,96000
-DcsjsoundChannels=1,2,4,6,8
```

or default values specified in https://github.com/pavhofman/csjsound-provider/blob/4326a4d77201f24c4b39be7391b3b45bfd76c204/src/main/java/com/cleansine/sound/provider/SimpleMixerProvider.java#L32-L33 and
https://github.com/pavhofman/csjsound-provider/blob/4326a4d77201f24c4b39be7391b3b45bfd76c204/src/main/java/com/cleansine/sound/provider/SimpleMixerProvider.java#L36 are used.

If only default values are used, a limit condition during the check is applied 

```
IF rate < MAX_RATE_LIMIT || channels < MAX_CHANNELS_LIMIT
```

where MAX_RATE_LIMIT is https://github.com/pavhofman/csjsound-provider/blob/4326a4d77201f24c4b39be7391b3b45bfd76c204/src/main/java/com/cleansine/sound/provider/SimpleMixerProvider.java#L40 and MAX_CHANNELS_LIMIT is https://github.com/pavhofman/csjsound-provider/blob/4326a4d77201f24c4b39be7391b3b45bfd76c204/src/main/java/com/cleansine/sound/provider/SimpleMixerProvider.java#L41

For sample formats these combinations of valid_bits and store_bits are checked:
```
vec!((16, 16), (24, 24), (24, 32), (32, 32))
```

The following channel masks are sequentially checked:
1. Masks used by PortAudio (defined for up to 8 channels)
https://github.com/pavhofman/csjsound-wasapi/blob/dea6073ca979cb7c3e2e316907f86c4431a397d1/src/formats.rs#L70-L79
2. Sequential bitmask for any channels count (e.g. 0b0...011_1111 for channels = 6)
3. Zero channel mask (as required by some capture devices)

In addition, for mono and stereo formats the corresponding shorter WAVEFORMATEX format is checked, as required by WASAPI specs.



