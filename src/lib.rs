#![allow(non_snake_case)]

extern crate core;

use core::slice;
use std::error::Error;
use std::fs::File;
use std::io;

use ::function_name::named;
use jni::JNIEnv;
use jni::objects::{AutoArray, AutoPrimitiveArray, JClass, JObject, JString, JValue, ReleaseMode};
use jni::signature::TypeSignature;
use jni::sys::{jboolean, jbyteArray, jint, jintArray, jlong, jobject};
use log::{debug, error, info, LevelFilter, trace};
use simplelog::{ConfigBuilder, format_description, WriteLogger};
use wasapi::Direction;

use wasapi_impl::*;

mod wasapi_impl;

pub type Res<T> = Result<T, Box<dyn Error>>;

pub struct MixerDesc {
    deviceID: String,
    max_lines: usize,
    name: String,
    description: String,
}

#[derive(Debug, Clone)]
pub struct Format {
    validbits: i32,
    frame_bytes: i32,
    channels: i32,
    rate: i32,
    is_signed: bool,
    is_big_endian: bool,
}

const ADD_FORMAT_METHOD: &'static str = "addFormat";
const ADD_FORMAT_SIGNATURE: &'static str = "(Ljava/util/Vector;IIIIIZZ)V";


#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixerProvider_nInit
(env: JNIEnv, _clazz: JClass, logLevelID: jint, logTarget: JString,
 jrates: jintArray, jchannels: jintArray, maxRatesLimit: jint, maxChannelsLimit: jint) -> jboolean {
    let log_target_str = get_string(env, logTarget);
    let log_level: LevelFilter = match logLevelID as usize {
        // same constants as in the java provider
        0 => LevelFilter::Error,
        1 => LevelFilter::Warn,
        2 => LevelFilter::Info,
        3 => LevelFilter::Debug,
        4 => LevelFilter::Trace,
        _ => LevelFilter::Error,
    };
    let format = format_description!("[hour]:[minute]:[second].[subsecond]");
    let config = ConfigBuilder::new()
        .set_time_format_custom(format)
        .build();

    match log_target_str.as_str() {
        "stdout" => {
            let _ = WriteLogger::init(log_level, config, io::stdout());
        }
        "stderr" => {
            let _ = WriteLogger::init(log_level, config, io::stderr());
        }
        other => {
            let file = match File::create(other) {
                Ok(file) => { file }
                Err(err) => {
                    error!("{}: Failed to create log file {}: {}", function_name!(), other, err);
                    return 0 as jboolean;
                }
            };
            let _ = WriteLogger::init(log_level, config, file);
        }
    }
    // let _ = CombinedLogger::init(vec![
    //     //SimpleLogger::new(level, config.clone()),
    //     //WriteLogger::new(level, config, targetWritable),
    //     ,
    // ]);

    trace!("{}", function_name!());

    let rates = from_jint_array(env, jrates);
    debug!("Received rates to test: {:?}", rates);
    let channels = from_jint_array(env, jchannels);
    debug!("Received channels to test: {:?}", channels);

    let accepted_combination: Box<dyn Fn(usize, usize) -> bool> = if maxChannelsLimit > 0 && maxRatesLimit > 0 {
        // limits were assigned
        debug!("Received max rate {} and max channels {} to limit test combinations", maxRatesLimit, maxChannelsLimit);
        Box::new(|rate: usize, channels: usize| rate <= maxRatesLimit as usize || channels <= maxChannelsLimit as usize)
    } else {
        // no limits, all combinations accepted
        debug!("Received no max rate and max channels limits, will test all combinations");
        Box::new(|rate: usize, channels: usize| true)
    };

    fill_format_variants(rates, channels, accepted_combination);

    return match do_initialize_wasapi() {
        Ok(_) => {
            info!("Lib initialized");
            1 as jboolean
        }
        Err(err) => {
            error!("{}: WASAPI init failed: {}", function_name!(), err);
            0 as jboolean
        }
    };
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nGetFormats
(JNIEnv *env, jclass clazz, jstring deviceID, jboolean isSource, jobject formats)
*/
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nGetFormats
(env: JNIEnv, clazz: JClass, deviceID: JString, isSource: jboolean, formatsVec: JObject) {
    let deviceIDStr = get_string(env, deviceID);

    let formats = match do_get_formats(deviceIDStr, &get_direction(isSource)) {
        Ok(formats) => formats,
        Err(err) => {
            error!("{}: get_fmts failed: {:?}\n", function_name!(), err);
            return;
        }
    };
    let signature = TypeSignature::from_str(&ADD_FORMAT_SIGNATURE).unwrap();
    for format in formats {
        /*
            private static void addFormat(Vector<AudioFormat> v, int bits, int frameBytes, int channels,
                                          int rate, int encoding, boolean isSigned, boolean isBigEndian)
         */
        match env.call_static_method_unchecked(clazz,
                                               (clazz, ADD_FORMAT_METHOD, ADD_FORMAT_SIGNATURE),
                                               signature.ret.clone(),
                                               &[
                                                   JValue::from(formatsVec),
                                                   JValue::Int(format.validbits),
                                                   JValue::Int(format.frame_bytes),
                                                   JValue::Int(format.channels),
                                                   JValue::Int(format.rate),
                                                   JValue::Int(0), // fixed PCM
                                                   JValue::from(format.is_signed),
                                                   JValue::from(format.is_big_endian),
                                               ]) {
            Ok(_) => {}
            Err(err) => {
                error!("{}: Calling method addFormat failed: {:?}\n", function_name!(), err);
                return;
            }
        }
    }
}


/*
JNIEXPORT jlong JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nOpen
	(JNIEnv* env, jclass clazz, jstring deviceID, jboolean isSource,
	jint enc, jint rate, jint sampleSignBits, jint frameBytes, jint channels,
	jboolean isSigned, jboolean isBigEndian, jint bufferBytes)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nOpen
(env: JNIEnv, _clazz: JClass, deviceID: JString, isSource: jboolean,
 _enc: jint, rate: jint, sampleSignBits: jint, frameBytes: jint, channels: jint,
 _isSigned: jboolean, _isBigEndian: jboolean, bufferBytes: jint) -> jlong {
    let deviceIDStr = get_string(env, deviceID);
    let rtd: RuntimeData = match do_open_dev(deviceIDStr, &get_direction(isSource), rate as usize,
                                             sampleSignBits as usize, frameBytes as usize,
                                             channels as usize, bufferBytes as usize) {
        Ok(rtd) => rtd,
        Err(err) => {
            error!("{}: open_dev failed: {:?}\n", function_name!(), err);
            // SimpleDataLine.doOpen checks for 0 (= NULL)
            return 0;
        }
    };
    // getting the pointer
    get_rtd_box_ptr(rtd)
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nStart
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nStart
(_env: JNIEnv, _clazz: JClass, nativePtr: jlong, isSource: jboolean) {
    trace!("{}", function_name!());
    let rtd = get_rtd(nativePtr);
    match do_start(rtd, &get_direction(isSource)) {
        Ok(_) => {}
        Err(err) => {
            error!("{}: start failed: {:?}\n", function_name!(), err);
        }
    }
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nStop
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nStop
(_env: JNIEnv, _clazz: JClass, nativePtr: jlong, isSource: jboolean) {
    trace!("{}", function_name!());
    let rtd = get_rtd(nativePtr);
    match do_stop(rtd, &get_direction(isSource)) {
        Ok(_) => {}
        Err(err) => {
            error!("{}: stop failed: {:?}\n", function_name!(), err);
        }
    }
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nClose
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nClose
(_env: JNIEnv, _clazz: JClass, nativePtr: jlong, isSource: jboolean) {
    trace!("{}", function_name!());

    // need to release the allocated memory => getting the box
    let rtd = get_rtd_box(nativePtr);
    match do_close(&rtd, &get_direction(isSource)) {
        Ok(_) => {}
        Err(err) => {
            error!("{}: closing failed: {:?}\n", function_name!(), err);
        }
    }
    // freeing rtd from heap
    drop(rtd);
}


/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nWrite
	(JNIEnv *env, jclass clazz, jlong nativePtr, jbyteArray jData, jint offset, jint len)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nWrite
(env: JNIEnv, _clazz: JClass, nativePtr: jlong, jData: jbyteArray, offset: jint, len: jint) -> jint {
    trace!("{}", function_name!());
    let rtd = get_rtd(nativePtr);
    // warn - AutoPrimitiveArray disables GC in java until the array is dropped in rust
    let jarr: AutoPrimitiveArray = env.get_primitive_array_critical(jData, ReleaseMode::NoCopyBack).unwrap();
    let size = jarr.size().unwrap() as usize;
    let items: &[u8] = unsafe { slice::from_raw_parts(jarr.as_ptr() as *const u8, size) };
    let cnt = match do_write(rtd, items, offset as usize, len as usize) {
        Ok(cnt) => cnt,
        Err(e) => {
            error!("{}: Writing failed: {:?}", function_name!(), e);
            return -1 as jint;
        }
    };
    cnt as jint
}


/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nRead
	(JNIEnv* env, jclass clazz, jlong nativePtr, jbyteArray jData, jint offset, jint len)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nRead
(env: JNIEnv, _clazz: JClass, nativePtr: jlong, jData: jbyteArray, offset: jint, len: jint) -> jint {
    trace!("{}", function_name!());
    let rtd = get_rtd(nativePtr);
    let jarr: AutoPrimitiveArray = env.get_primitive_array_critical(jData, ReleaseMode::CopyBack).unwrap();
    let size = jarr.size().unwrap() as usize;
    let items: &mut [u8] = unsafe { slice::from_raw_parts_mut(jarr.as_ptr() as *mut u8, size) };
    let cnt = match do_read(rtd, items, offset as usize, len as usize) {
        Ok(cnt) => cnt,
        Err(e) => {
            error!("{}: Reading failed: {:?}", function_name!(), e);
            return -1 as jint;
        }
    };
    cnt as jint
}


/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nGetBufferBytes
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nGetBufferBytes
(_env: JNIEnv, _clazz: JClass, nativePtr: jlong, isSource: jboolean) -> jint {
    let dir = get_direction(isSource);
    trace!("{} {}", function_name!(), dir);
    let rtd = get_rtd(nativePtr);
    let bytes = match do_get_buffer_bytes(rtd, &dir) {
        Ok(size) => size,
        Err(e) => {
            error!("{}: Getting buffer_bytes failed: {:?}", function_name!(), e);
            return 0 as jint;
        }
    };
    trace!("{} {}: returning {}", function_name!(), dir, bytes);
    bytes as jint
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nDrain
	(JNIEnv* env, jclass clazz, jlong nativePtr)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nDrain
(_env: JNIEnv, _clazz: JClass, nativePtr: jlong) {
    trace!("{}", function_name!());
    let rtd = get_rtd(nativePtr);
    do_drain(rtd);
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nFlush
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nFlush
(_env: JNIEnv, _clazz: JClass, nativePtr: jlong, _isSource: jboolean) {
    trace!("{}", function_name!());
    let rtd = get_rtd(nativePtr);
    do_flush(rtd);
}


/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nGetAvailBytes
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nGetAvailBytes
(_env: JNIEnv, _clazz: JClass, nativePtr: jlong, isSource: jboolean) -> jint {
    let dir = get_direction(isSource);
    trace!("{} {}", function_name!(), dir);

    let rtd = get_rtd(nativePtr);
    let bytes = match do_get_avail_bytes(rtd, &dir) {
        Ok(size) => size,
        Err(e) => {
            error!("{}: Getting avail_bytes failed: {:?}", function_name!(), e);
            return 0 as jint;
        }
    };
    trace!("{} {}: returning {}", function_name!(), dir, bytes);
    bytes as jint
}


/*
JNIEXPORT jlong JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nGetBytePos
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource, jlong javaBytePos)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nGetBytePos
(_env: JNIEnv, _clazz: JClass, nativePtr: jlong, isSource: jboolean, javaBytePos: jlong) -> jlong {
    trace!("{}", function_name!());
    let rtd = get_rtd(nativePtr);
    let bytes = match do_get_byte_pos(rtd, &get_direction(isSource), javaBytePos as u64) {
        Ok(size) => size,
        Err(e) => {
            error!("{}: Getting avail_bytes failed: {:?}", function_name!(), e);
            return 0 as jlong;
        }
    };
    trace!("{}: returning {}", function_name!(), bytes);
    bytes as jlong
}

/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixerProvider_nGetMixerCnt
	(JNIEnv *env, jclass clazz)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixerProvider_nGetMixerCnt
(_env: JNIEnv, _clazz: JClass) -> jint {
    trace!("{}", function_name!());
    let cnt = match do_get_device_cnt() {
        Ok(cnt) => cnt,
        Err(e) => {
            error!("{}: Getting DeviceCollection failed: {:?}", function_name!(), e);
            return 0 as jint;
        }
    };
    cnt as jint
}


const MIXER_INFO_CLASS: &'static str = "com/cleansine/sound/provider/SimpleMixerInfo";
const MIXER_INFO_SIGNATURE: &'static str = "(ILjava/lang/String;ILjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V";

/*
JNIEXPORT jobject JNICALL Java_com_cleansine_sound_provider_SimpleMixerProvider_nCreateMixerInfo
	(JNIEnv *env, jclass clazz, jint idx)
 */
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixerProvider_nCreateMixerInfo
(env: JNIEnv, _clazz: JClass, idx: jint) -> jobject {
    trace!("{}", function_name!());
    let desc = match do_get_mixer_desc(idx as u32) {
        Ok(desc) => desc,
        Err(err) => {
            error!("{}: Getting MixerDesc for idx {} failed: {:?}", function_name!(), idx, err);
            return JObject::null().into_inner();
        }
    };

    let info_cls = match env.find_class(MIXER_INFO_CLASS) {
        Ok(c) => c,
        Err(err) => {
            error!("{}: info_cls class not found: {:?}\n", function_name!(), err);
            return JObject::null().into_inner();
        }
    };

    let deviceID = env.new_string(desc.deviceID).unwrap();
    let name = env.new_string(desc.name).unwrap();
    let description = env.new_string(desc.description).unwrap();
    let vendor = env.new_string("WASAPI").unwrap();
    //idx, deviceID, desc.maxLines, name, vendor, description
    let obj = match env.new_object(info_cls,
                                   MIXER_INFO_SIGNATURE,
                                   &[JValue::Int(idx),
                                       JValue::from(deviceID),
                                       JValue::Int(desc.max_lines as jint),
                                       JValue::from(name),
                                       JValue::from(vendor),
                                       JValue::from(description)
                                   ]) {
        Ok(obj) => obj,
        Err(err) => {
            error!("{}: Cannot instantiate SimpleMixerInfo: {:?}", function_name!(), err);
            return JObject::null().into_inner();
        }
    };
    trace!("{} done.", function_name!());
    obj.into_inner()
}


fn get_direction(isSource: jboolean) -> Direction {
    if isSource > 0 { Direction::Render } else { Direction::Capture }
}


fn get_rtd_box_ptr(rtd: RuntimeData) -> jlong {
    let rtd_box: Box<RuntimeData> = Box::new(rtd);
    let raw: *mut RuntimeData = Box::into_raw(rtd_box);
    raw as jlong
}


// java -> rust
// static - HACK!
fn get_rtd(ptr: jlong) -> &'static mut RuntimeData {
    // TODO - check for ptr != 0
    let rtd: &mut RuntimeData = unsafe { jlong_to_pointer::<RuntimeData>(ptr).as_mut().unwrap() };
    rtd
}

fn get_rtd_box(ptr: jlong) -> Box<RuntimeData> {
    // TODO - check for ptr != 0
    // Box destructor will free the allocated heap memory
    let rtd_box = unsafe { Box::from_raw(jlong_to_pointer::<RuntimeData>(ptr)) };
    rtd_box
}

#[cfg(target_pointer_width = "32")]
pub unsafe fn jlong_to_pointer<T>(ptr: jlong) -> *mut T { (ptr as u32) as *mut T }

#[cfg(target_pointer_width = "64")]
pub unsafe fn jlong_to_pointer<T>(ptr: jlong) -> *mut T {
    ptr as *mut T
}

fn get_string(env: JNIEnv, str: JString) -> String {
    env.get_string(str)
        .expect("Couldn't get java string!")
        .into()
}

fn from_jint_array(env: JNIEnv, jarr: jintArray) -> Vec<usize> {
    let auto_ptr: AutoArray<jint> = env.get_int_array_elements(jarr, ReleaseMode::NoCopyBack).unwrap();
    let ptr = auto_ptr.as_ptr();
    let cnt = auto_ptr.size().unwrap() as usize;
    let mut values = vec![0; cnt];

    for i in 0..cnt {
        values[i] = unsafe { *ptr.offset(i as isize) } as usize;
    }
    values
}
