#![allow(non_snake_case)]

use std::sync::atomic::{AtomicBool, Ordering};

use ::function_name::named;
use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::signature::TypeSignature;
use jni::sys::{jboolean, jbyteArray, jint, jlong, jobject, jstring};
use log::{error, info, trace, warn};

struct MixerDesc {
    // used when walking descs to find a desc with required idx
    down_counter: usize,
    deviceID: String,
    max_lines: usize,
    name: String,
    description: String,
}


const ADD_FORMAT_METHOD: &'static str = "addFormat";
const ADD_FORMAT_SIGNATURE: &'static str = "(Ljava/util/Vector;IIIIIZZ)V";

/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nGetFormats
(JNIEnv *env, jclass clazz, jstring deviceID, jboolean isSource, jobject formats)
*/
#[named]
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nGetFormats
(env: JNIEnv, clazz: JClass, deviceID: JString, isSource: jboolean, formats: JObject) {
    let parsed = TypeSignature::from_str(&ADD_FORMAT_SIGNATURE).unwrap();

    /*
        private static void addFormat(Vector<AudioFormat> v, int bits, int frameBytes, int channels,
                                      int rate, int encoding, boolean isSigned, boolean isBigEndian)
     */
    match env.call_static_method_unchecked(clazz, (clazz, ADD_FORMAT_METHOD, ADD_FORMAT_SIGNATURE), parsed.ret.clone(), &[
        JValue::from(formats),
        JValue::Int(16),
        JValue::Int(2),
        JValue::Int(2),
        JValue::Int(-1),    // unspecified rate
        JValue::Int(0), // PCM
        JValue::from(true), // S
        JValue::from(false), // LE
    ]) {
        Ok(_) => {}
        Err(err) => {
            error!("{}: Calling method addFormat failed: {:?}\n", function_name!(), err);
            return;
        }
    }

    match env.call_static_method_unchecked(clazz, (clazz, ADD_FORMAT_METHOD, ADD_FORMAT_SIGNATURE), parsed.ret, &[
        JValue::from(formats),
        JValue::Int(24),
        JValue::Int(2),
        JValue::Int(2),
        JValue::Int(-1),    // unspecified rate
        JValue::Int(0), // PCM
        JValue::from(true),
        JValue::from(false), //LE
    ]) {
        Ok(_) => {}
        Err(err) => {
            error!("{}: Calling method addFormat failed: {:?}\n", function_name!(), err);
            return;
        }
    }
}


/*
JNIEXPORT jlong JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nOpen
	(JNIEnv* env, jclass clazz, jstring deviceID, jboolean isSource,
	jint enc, jint rate, jint sampleSignBits, jint frameBytes, jint channels,
	jboolean isSigned, jboolean isBigEndian, jint bufferBytes)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nOpen
(env: JNIEnv, clazz: JClass, deviceID: JString, isSource: jboolean,
 enc: jint, rate: jint, sampleSignBits: jint, frameBytes: jint, channels: jint,
 isSigned: jboolean, isBigEndian: jboolean, bufferBytes: jint) -> jlong {
    -1
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nStart
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nStart
(env: JNIEnv, clazz: JClass, nativePtr: jlong, isSource: jboolean) -> jint {
    -1
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nStop
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nStop
(env: JNIEnv, clazz: JClass, nativePtr: jlong, isSource: jboolean) -> jint {
    -1
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nClose
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nClose
(env: JNIEnv, clazz: JClass, nativePtr: jlong, isSource: jboolean) -> jint {
    -1
}


/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nWrite
	(JNIEnv *env, jclass clazz, jlong nativePtr, jbyteArray jData, jint offset, jint len)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nWrite
(env: JNIEnv, clazz: JClass, nativePtr: jlong, jData: jbyteArray, offset: jint, len: jint) -> jint {
    -1
}


/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nRead
	(JNIEnv* env, jclass clazz, jlong nativePtr, jbyteArray jData, jint offset, jint len)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nRead
(env: JNIEnv, clazz: JClass, nativePtr: jlong, jData: jbyteArray, offset: jint, len: jint) -> jint {
    -1
}


/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nGetBufferSize
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nGetBufferSize
(env: JNIEnv, clazz: JClass, nativePtr: jlong, isSource: jboolean) -> jint {
    -1
}


/*
JNIEXPORT jboolean JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nDrain
	(JNIEnv* env, jclass clazz, jlong nativePtr)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nDrain
(env: JNIEnv, clazz: JClass, nativePtr: jlong) -> jboolean {
    0
}


/*
JNIEXPORT void JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nFlush
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nFlush
(env: JNIEnv, clazz: JClass, nativePtr: jlong, isSource: jboolean) {}


/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nGetAvailBytes
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nGetAvailBytes
(env: JNIEnv, clazz: JClass, nativePtr: jlong, isSource: jboolean) -> jint {
    -1
}


/*
JNIEXPORT jlong JNICALL Java_com_cleansine_sound_provider_SimpleMixer_nGetBytePos
	(JNIEnv* env, jclass clazz, jlong nativePtr, jboolean isSource, jlong javaBytePos)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixer_nGetBytePos
(env: JNIEnv, clazz: JClass, nativePtr: jlong, isSource: jboolean, javaBytePos: jlong) -> jlong {
    -1
}

/*
JNIEXPORT jint JNICALL Java_com_cleansine_sound_provider_SimpleMixerProvider_nGetMixerCnt
	(JNIEnv *env, jclass clazz)
 */
#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixerProvider_nGetMixerCnt
(env: JNIEnv, clazz: JClass) -> jint {
    1
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
(env: JNIEnv, clazz: JClass, idx: jint) -> jobject {
    let info_cls = match env.find_class(MIXER_INFO_CLASS) {
        Ok(c) => c,
        Err(err) => {
            error!("{}: info_cls class not found: {:?}\n", function_name!(), err);
            return JObject::null().into_inner();
        }
    };

    // TODO vratit desc s parametrem idx
    let desc = MixerDesc {
        down_counter: idx as usize,
        deviceID: "DEVICE".to_string(),
        max_lines: 1,
        name: "NAME".to_string(),
        description: "DESC".to_string(),
    };
    // if (doFillDesc(&desc)) {
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


#[no_mangle]
pub extern "system" fn Java_com_cleansine_sound_provider_SimpleMixerProvider_nInit
(env: JNIEnv, _clazz: JClass) -> jboolean {
    println!("LIB INITIALIZED!");
    1 as jboolean
}
