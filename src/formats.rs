use lazy_static::lazy_static;
use wasapi::{SampleType, WaveFormat};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Format {
    pub validbits: i32,
    pub frame_bytes: i32,
    pub channels: i32,
    pub rate: i32,
}


impl From<WaveFormat> for Format {
    fn from(wvfmt: WaveFormat) -> Self {
        Format {
            validbits: wvfmt.get_validbitspersample() as i32,
            frame_bytes: ((wvfmt.get_bitspersample() / 8) as i32) * wvfmt.get_nchannels() as i32,
            channels: wvfmt.get_nchannels() as i32,
            rate: wvfmt.get_samplespersec() as i32,
        }
    }
}

lazy_static! {
    pub static ref WV_FMTS_BY_FORMAT: Mutex<HashMap<Format, Vec<WaveFormat>>> = Mutex::new(HashMap::new());
}

const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;
const SPEAKER_LOW_FREQUENCY: u32 = 0x8;
const SPEAKER_BACK_LEFT: u32 = 0x10;
const SPEAKER_BACK_RIGHT: u32 = 0x20;
const SPEAKER_FRONT_LEFT_OF_CENTER: u32 = 0x40;
const SPEAKER_FRONT_RIGHT_OF_CENTER: u32 = 0x80;
const SPEAKER_BACK_CENTER: u32 = 0x100;
const SPEAKER_SIDE_LEFT: u32 = 0x200;
const SPEAKER_SIDE_RIGHT: u32 = 0x400;
const _SPEAKER_TOP_CENTER: u32 = 0x800;
const _SPEAKER_TOP_FRONT_LEFT: u32 = 0x1000;
const _SPEAKER_TOP_FRONT_CENTER: u32 = 0x2000;
const _SPEAKER_TOP_FRONT_RIGHT: u32 = 0x4000;
const _SPEAKER_TOP_BACK_LEFT: u32 = 0x8000;
const _SPEAKER_TOP_BACK_CENTER: u32 = 0x10000;
const _SPEAKER_TOP_BACK_RIGHT: u32 = 0x20000;


const SPEAKER_STEREO: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
const SPEAKER_QUAD: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT |
    SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT;
const SPEAKER_SURROUND: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT |
    SPEAKER_FRONT_CENTER | SPEAKER_BACK_CENTER;
const SPEAKER_5POINT1: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT |
    SPEAKER_FRONT_CENTER | SPEAKER_LOW_FREQUENCY |
    SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT;
const SPEAKER_7POINT1: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT |
    SPEAKER_FRONT_CENTER | SPEAKER_LOW_FREQUENCY |
    SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT |
    SPEAKER_FRONT_LEFT_OF_CENTER | SPEAKER_FRONT_RIGHT_OF_CENTER;
const SPEAKER_5POINT1_SURROUND: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT |
    SPEAKER_FRONT_CENTER | SPEAKER_LOW_FREQUENCY |
    SPEAKER_SIDE_LEFT | SPEAKER_SIDE_RIGHT;
const SPEAKER_7POINT1_SURROUND: u32 = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT |
    SPEAKER_FRONT_CENTER | SPEAKER_LOW_FREQUENCY |
    SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT |
    SPEAKER_SIDE_LEFT | SPEAKER_SIDE_RIGHT;

const CHANNEL_MASKS: [&[u32]; 8] = [
    &[SPEAKER_FRONT_CENTER],
    &[SPEAKER_STEREO],
    &[SPEAKER_STEREO | SPEAKER_LOW_FREQUENCY],
    &[SPEAKER_QUAD, SPEAKER_SURROUND],
    &[SPEAKER_QUAD | SPEAKER_LOW_FREQUENCY, SPEAKER_SURROUND | SPEAKER_LOW_FREQUENCY],
    &[SPEAKER_5POINT1, SPEAKER_5POINT1_SURROUND],
    &[SPEAKER_5POINT1 | SPEAKER_BACK_CENTER, SPEAKER_5POINT1_SURROUND | SPEAKER_BACK_CENTER],
    &[SPEAKER_7POINT1, SPEAKER_7POINT1_SURROUND],
];

pub fn init_format_variants<T>(rate_variants: Vec<usize>, channels_variants: Vec<usize>, accepted_combination: T)
    where T: Fn(usize, usize) -> bool {
    let valid_store_bits_variants: Vec<(usize, usize)> = vec!((16, 16), (24, 24), (24, 32), (32, 32));
    for rate in rate_variants {
        for &channels in &channels_variants {
            // upper limit on rate x channels combination
            if accepted_combination(rate, channels) {
                for &(validbits, storebits) in &valid_store_bits_variants {
                    let fmt = Format {
                        validbits: validbits as i32,
                        frame_bytes: (channels * storebits / 8) as i32,
                        channels: channels as i32,
                        rate: rate as i32,
                    };
                    let mut map = WV_FMTS_BY_FORMAT.lock().unwrap();
                    map.insert(fmt, get_possible_formats(storebits, validbits, rate, channels));
                }
            }
        }
    }
}

pub fn get_possible_formats(storebits: usize, validbits: usize, rate: usize, channels: usize) -> Vec<WaveFormat> {
    let mut wvformats = Vec::new();

    //WAVEXTENSIBLE versions:

    let wvformat = WaveFormat::new(
        storebits,
        validbits,
        &SampleType::Int,
        rate,
        channels,
        None,
    );

    // Portaudio channel masks are most likely, adding them first
    if channels <= CHANNEL_MASKS.len() {
        for &mask in CHANNEL_MASKS[channels - 1] {
            let mut cloned = wvformat.clone();
            cloned.wave_fmt.dwChannelMask = mask;
            wvformats.push(cloned);
        }
    }

    if channels > 2 {
        // wasapi-rs format mask (differs from CHANNEL_MASKS starting at channels 3)
        wvformats.push(wvformat.clone());
    }

    // adding format with zero channel mask (some capture devices require that)
    let mut zero_chmask_format = wvformat.clone();
    zero_chmask_format.wave_fmt.dwChannelMask = 0;
    wvformats.push(zero_chmask_format);

    // adding WAVEX format for legacy formats (see https://docs.microsoft.com/en-us/windows/win32/coreaudio/device-formats#specifying-the-device-format)
    if wvformat.get_nchannels() <= 2 && wvformat.get_bitspersample() <= 16 {
        wvformats.push(wvformat.to_waveformatex().unwrap());
    }
    wvformats
}