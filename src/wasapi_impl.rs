use std::{error, fmt, thread};
use std::cmp;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError, unbounded};
use log::{debug, error, trace, warn};
use wasapi::{AudioClient, BufferFlags, Device, DeviceCollection, Direction, DisconnectReason, Handle, initialize_sta, ShareMode, WaveFormat};
use windows::core::PCWSTR;
use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;

use crate::{MixerDesc, Res};
use crate::formats::{Format, get_possible_formats, WV_FMTS_BY_FORMAT};


// defined in JAVA
const NOT_SPECIFIED: i32 = -1;

//#[derive(Debug)]
pub struct RuntimeData {
    device_id: String,
    device_name: String,
    dir: Direction,
    play_tx_dev: Option<Sender<Vec<u8>>>,
    play_draining_rx_dev: Option<Receiver<Vec<u8>>>,
    capt_rx_dev: Option<Receiver<(u64, Vec<u8>)>>,
    capt_tx_prealloc: Option<Sender<Vec<u8>>>,
    rx_state_dev: Receiver<DeviceState>,
    rx_disconnectreason: Receiver<Disconnected>,
    bufferfill_bytes: Arc<AtomicUsize>,
    chunk_frames: usize,
    frame_bytes: usize,
    leftovers: Vec<u8>,
    leftovers_pos: Arc<AtomicUsize>,
    start_signal: Arc<AtomicBool>,
    stop_signal: Arc<AtomicBool>,
    exit_signal: Arc<AtomicBool>,
    capt_last_chunk_nbr: u64,
    //outer_file: Box<dyn Write>,
}

#[derive(Clone, Debug)]
pub enum Disconnected {
    FormatChange,
    Error,
}

#[derive(Debug)]
pub struct DeviceError {
    desc: String,
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.desc)
    }
}

impl error::Error for DeviceError {
    fn description(&self) -> &str {
        &self.desc
    }
}

impl DeviceError {
    pub fn new(desc: &str) -> Self {
        DeviceError {
            desc: desc.to_owned(),
        }
    }
}

pub struct PlaySyncData {
    pub rx_dev: Receiver<Vec<u8>>,
    pub tx_cb: Sender<Disconnected>,
    pub wasapi_bufferfill_bytes: Arc<AtomicUsize>,
    pub start_signal: Arc<AtomicBool>,
    pub stop_signal: Arc<AtomicBool>,
    pub exit_signal: Arc<AtomicBool>,
}

pub struct CaptSyncData {
    pub tx_dev: Sender<(u64, Vec<u8>)>,
    pub rx_prealloc: Receiver<Vec<u8>>,
    pub tx_cb: Sender<Disconnected>,
    pub wasapi_bufferfill_bytes: Arc<AtomicUsize>,
    pub start_signal: Arc<AtomicBool>,
    pub stop_signal: Arc<AtomicBool>,
    pub exit_signal: Arc<AtomicBool>,
}

enum DeviceState {
    Ok(usize),
    Error(String),
}


pub fn do_initialize_wasapi() -> Res<()> {
    initialize_sta()?;
    Ok(())
}

pub fn do_get_device_cnt() -> Res<u32> {
    let (playCollection, captCollection) = get_colls()?;
    let mut cnt = playCollection.get_nbr_devices()?;
    cnt = cnt + captCollection.get_nbr_devices()?;
    Ok(cnt)
}

fn get_colls() -> Res<(DeviceCollection, DeviceCollection)> {
    let playCollection = DeviceCollection::new(&Direction::Render)?;
    let captCollection = DeviceCollection::new(&Direction::Capture)?;
    Ok((playCollection, captCollection))
}

pub fn do_get_mixer_desc(idx: u32) -> Res<MixerDesc> {
    let (dev, _) = get_device_at_idx(idx)?;
    let name = dev.get_friendlyname()?;
    let desc = MixerDesc {
        // for now using idx
        deviceID: idx.to_string(),
        max_lines: 1,
        name: format!("EXCL: {}", name),
        description: dev.get_description()?,
    };
    Ok(desc)
}

fn get_device_at_idx(idx: u32) -> Res<(Device, Direction)> {
    let (playCollection, captCollection) = get_colls()?;
    let playCnt = playCollection.get_nbr_devices()?;
    let (coll, colIdx, dir) = if playCnt > idx {
        (playCollection, idx, Direction::Render)
    } else {
        (captCollection, idx - playCnt, Direction::Capture)
    };
    let dev = coll.get_device_at_index(colIdx)?;
    Ok((dev, dir))
}

fn get_device_by_id(device_id: &str) -> Res<(Device, Direction)> {
    let idx = device_id.parse::<u32>()?;
    let (dev, dir) = get_device_at_idx(idx)?;
    Ok((dev, dir))
}

pub fn do_get_formats(device_id: String, dir: &Direction) -> Res<Vec<Format>> {
    let (dev, dev_dir) = get_device_by_id(&device_id)?;
    let fmts = if *dir == dev_dir { get_device_formats(dev)? } else { vec!() };
    Ok(fmts)
}

fn get_supported_format(client: &AudioClient, dev_name: &str, wvformat: &WaveFormat) -> Option<WaveFormat> {
    let result = match client.is_supported(wvformat, &ShareMode::Exclusive) {
        Ok(None) => {
            debug!("Device {} supports format {:?}", dev_name, *wvformat);
            Some(wvformat.clone())
        }
        Ok(Some(similar_wvfmt)) => {
            // WASAPI specs say this should not happen in exclusive mode
            debug!("Device {} supports similar format {:?}", dev_name, similar_wvfmt);
            Some(similar_wvfmt)
        }
        Err(err) => {
            debug!("Device {} does not support format {:?}: {}", dev_name, wvformat, err);
            None
        }
    };
    result
}

fn get_device_formats(dev: Device) -> Res<Vec<Format>> {
    let mut formats = Vec::new();
    let dev_name = dev.get_friendlyname()?;
    let client = dev.get_iaudioclient()?;
    let mut supported_validbits: HashSet<i32> = HashSet::new();
    for (_format, wvformats) in &*WV_FMTS_BY_FORMAT.lock().unwrap() {
        //adding only first supported wvformat for the given format

        for wvformat in wvformats {
            // wvformat is wavextensible from wasapi-rs
            match get_supported_format(&client, &dev_name, wvformat) {
                Some(ok_wvformat) => {
                    let ok_format = Format::from(ok_wvformat);
                    supported_validbits.insert(ok_format.validbits);
                    formats.push((ok_format).clone());
                    // no more wvformat checks for this _format
                    break;
                }
                None => {}
            }
        }
    }
    // adding formats with NOT_SPECIFIED channels and rate because only predefined values are checked
    for validbits in supported_validbits {
        let format = Format {
            validbits,
            frame_bytes: NOT_SPECIFIED,
            channels: NOT_SPECIFIED,
            rate: NOT_SPECIFIED,
        };
        formats.push(format);
    }
    Ok(formats)
}


pub fn do_open_dev(device_id: String, dir: &Direction, rate: usize, validbits: usize, frame_bytes: usize,
                   channels: usize, buffer_bytes: usize) -> Res<RuntimeData> {
    let (_device, device_name, audio_client) = get_device_details(&device_id, dir)?;
    debug!("Opening {} device {}: rate: {}, validbits: {}, frame_bytes: {}, channels: {}, buffer_bytes: {}",
        dir, device_name, rate, validbits, frame_bytes, channels, buffer_bytes);
    let (_def_period, min_period) = audio_client.get_periods()?;
    debug!(
        "{}: default period {}, min period {}",
        dir,
        _def_period, min_period
    );

    let is_playback = *dir == Direction::Render;

    // 30 ms
    let dev_period = cmp::max(30 * 10_000, min_period);
    // this code assumes device.Initialize will use closely similar buffer to dev_period
    let estimated_chunk_frames = (rate as i64 * dev_period / 10_000_000) as usize;
    let chunks = ((buffer_bytes as f32 / frame_bytes as f32) / estimated_chunk_frames as f32) as usize;
    trace!("{}: Using {} chunks in buffer => total estimated {} bytes", dir, chunks, chunks * estimated_chunk_frames * frame_bytes);
    let (play_tx_dev, play_rx_dev, play_draining_rx_dev) = if is_playback {
        let (tx, rx) = bounded(chunks);
        (Some(tx), Some(rx.clone()), Some(rx))
    } else {
        (None, None, None)
    };
    let (capt_tx_dev, capt_rx_dev) = if is_playback {
        (None, None)
    } else {
        let (tx, rx) = bounded(chunks);
        (Some(tx), Some(rx))
    };

    let (capt_tx_prealloc, capt_rx_prealloc) = if is_playback {
        (None, None)
    } else {
        let (tx, rx) = bounded(chunks + 2);
        // filling the channel with preallocated chunks
        // estimated_chunk_frames is just an estimate, keeping marging for possibly larger chunk_frames
        let prealloc_chunk_bytes = (1.5 * estimated_chunk_frames as f32 * frame_bytes as f32) as usize;
        for _ in 0..(chunks + 2) {
            let prealloc_chunk = vec![0u8; prealloc_chunk_bytes];
            tx.send(prealloc_chunk).unwrap();
        }
        (Some(tx), Some(rx))
    };


    let (tx_state_dev, rx_state_dev) = bounded(0);
    let (tx_disconnectreason, rx_disconnectreason) = unbounded();
    // for reporting delay
    let bufferfill_frames = Arc::new(AtomicUsize::new(0));
    let bufferfill_bytes_cloned = bufferfill_frames.clone();
    let device_id_cloned = device_id.clone();
    let dir_cloned = dir.clone();

    let start_signal = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::new(AtomicBool::new(false));
    let exit_signal = Arc::new(AtomicBool::new(false));

    let start_signal_cloned = start_signal.clone();
    let stop_signal_cloned = stop_signal.clone();
    let exit_signal_cloned = exit_signal.clone();

    // wasapi device loop
    // TODO - joining the thread somehow?
    let _innerhandle = thread::Builder::new()
        .name(format!("Wasapi{}Inner", dir).to_string())
        .spawn(move || {
            let (_device, audio_client, handle, client_buffer_frames) =
                match device_open(
                    &device_id_cloned,
                    &dir_cloned,
                    rate,
                    validbits,
                    frame_bytes,
                    channels,
                    dev_period,
                ) {
                    Ok((_device, audio_client, handle)) => {
                        let client_buffer_frames = audio_client.get_bufferframecount().unwrap() as usize;
                        tx_state_dev.send(DeviceState::Ok(client_buffer_frames)).unwrap_or(());
                        (_device, audio_client, handle, client_buffer_frames)
                    }
                    Err(err) => {
                        let msg = format!("PB: error: {}", err);
                        tx_state_dev.send(DeviceState::Error(msg)).unwrap_or(());
                        return;
                    }
                };
            trace!("client_buffer_frames: {}", client_buffer_frames);


            let result = if is_playback {
                let sync = PlaySyncData {
                    rx_dev: play_rx_dev.unwrap(),
                    tx_cb: tx_disconnectreason,
                    wasapi_bufferfill_bytes: bufferfill_bytes_cloned,
                    start_signal: start_signal_cloned,
                    stop_signal: stop_signal_cloned,
                    exit_signal: exit_signal_cloned,
                };
                playback_loop(
                    audio_client,
                    handle,
                    frame_bytes,
                    client_buffer_frames,
                    rate,
                    sync,
                )
            } else {
                let sync = CaptSyncData {
                    tx_dev: capt_tx_dev.unwrap(),
                    rx_prealloc: capt_rx_prealloc.unwrap(),
                    tx_cb: tx_disconnectreason,
                    wasapi_bufferfill_bytes: bufferfill_bytes_cloned,
                    start_signal: start_signal_cloned,
                    stop_signal: stop_signal_cloned,
                    exit_signal: exit_signal_cloned,
                };
                capture_loop(
                    audio_client,
                    handle,
                    frame_bytes,
                    client_buffer_frames,
                    rate,
                    sync,
                )
            };
            if let Err(err) = result {
                let msg = format!("{}: Looping failed with error: {:?}", dir_cloned, err);
                error!("{}", msg);
                tx_state_dev.send(DeviceState::Error(msg)).unwrap_or(());
            }
        })
        .unwrap();
    let real_chunk_frames = match rx_state_dev.recv() {
        Ok(DeviceState::Ok(frames)) => {
            frames
        }
        Ok(DeviceState::Error(msg)) => {
            return Err(Box::new(DeviceError { desc: msg }));
        }
        Err(err) => {
            return Err(Box::new(err));
        }
    };

    let rtd = RuntimeData {
        device_id,
        device_name,
        dir: dir.clone(),
        play_tx_dev,
        play_draining_rx_dev,
        capt_rx_dev,
        capt_tx_prealloc,
        rx_state_dev,
        rx_disconnectreason,
        // for reporting delay
        bufferfill_bytes: bufferfill_frames,
        chunk_frames: real_chunk_frames,
        frame_bytes,
        // 1 chunk of bytes
        leftovers: vec![0; real_chunk_frames * frame_bytes as usize],
        leftovers_pos: Arc::new(AtomicUsize::new(0)),
        start_signal,
        stop_signal,
        exit_signal,
        capt_last_chunk_nbr: 0,
        //outer_file: File::create("outer.raw").map(|f| Box::new(f) as Box<dyn Write>).unwrap(),
    };

    //trace!("RuntimeData: {:?}", rtd);
    debug!("Device ready and waiting");
    // loop {
    //     match rx_state_dev.try_recv() {
    //         Ok(DeviceState::Ok) => {}
    //         Ok(DeviceState::Error(err)) => {
    //             send_error_or_playbackformatchange(
    //                 &status_channel,
    //                 &rx_disconnectreason,
    //                 err,
    //             );
    //             break;
    //         }
    //         Err(TryRecvError::Empty) => {}
    //         Err(TryRecvError::Disconnected) => {
    //             send_error_or_playbackformatchange(
    //                 &status_channel,
    //                 &rx_disconnectreason,
    //                 "Inner playback thread has exited".to_string(),
    //             );
    //             break;
    //         }
    //     }
    // }
    Ok(rtd)
}

pub fn do_get_buffer_bytes(rtd: &RuntimeData, dir: &Direction) -> Res<usize> {
    check_direction_from_rt(rtd, dir, "get_buffer_bytes")?;
    // total bytes storable in all chunks in the FIFO tx_dev
    let chunks = if *dir == Direction::Render {
        rtd.play_tx_dev.as_ref().unwrap().capacity().unwrap()
    } else {
        rtd.capt_rx_dev.as_ref().unwrap().capacity().unwrap()
    };
    Ok(chunks * rtd.chunk_frames * rtd.frame_bytes)
}

pub fn do_start(rtd: &RuntimeData, dir: &Direction) -> Res<()> {
    check_direction_from_rt(rtd, dir, "start")?;
    rtd.start_signal.store(true, Ordering::Relaxed);
    Ok(())
}

pub fn do_stop(rtd: &RuntimeData, dir: &Direction) -> Res<()> {
    check_direction_from_rt(rtd, dir, "stop")?;
    rtd.stop_signal.store(true, Ordering::Relaxed);
    Ok(())
}

pub fn do_write(rtd: &mut RuntimeData, java_buffer: &[u8], offset: usize, data_len: usize) -> Res<usize> {
    trace!("PB: do_write: java_buffer {} bytes, offset {} bytes, writing {} bytes", java_buffer.len(), offset, data_len);

    let chunk_bytes = rtd.chunk_frames * rtd.frame_bytes;

    let mut data_to_write = &java_buffer[offset..(offset + data_len)];
    // rtd.outer_file.write_all(data_to_write);
    // rtd.outer_file.flush();
    // copying leftovers if any to the first chunk
    let mut leftovers_pos = rtd.leftovers_pos.load(Ordering::Relaxed);

    // if leftovers + data_to_write do not fill whole chunk
    if leftovers_pos + data_len < chunk_bytes {
        // just appending whole data_to_write to leftovers
        trace!("PB: write: leftovers_pos {} + data_len {} < chunk_bytes {}: only copying to leftovers",
        leftovers_pos, data_len, chunk_bytes);
        rtd.leftovers[leftovers_pos..leftovers_pos + data_len].copy_from_slice(&data_to_write);
        rtd.leftovers_pos.store(leftovers_pos + data_len, Ordering::Relaxed);
        // finished, no chunk to be sent to the inner thread
        return Ok(data_len);
    }

    // we have leftovers and enough new data to fill up new chunk
    if leftovers_pos > 0 {
        // allocating new chunk
        let mut chunk: Vec<u8> = Vec::with_capacity(chunk_bytes);
        unsafe { chunk.set_len(chunk_bytes); }

        chunk[0..leftovers_pos].copy_from_slice(&rtd.leftovers[0..leftovers_pos]);
        let bytes_from_data = chunk_bytes - leftovers_pos;
        chunk[leftovers_pos..].copy_from_slice(&data_to_write[0..bytes_from_data]);
        // leftovers are empty now
        leftovers_pos = 0;
        match rtd.play_tx_dev.as_ref().unwrap().send(chunk) {
            Ok(_) => {}
            Err(err) => {
                error!("{}", err.to_string());
                return Err(Box::new(err));
            }
        }
        // updating data_to_write
        data_to_write = &data_to_write[bytes_from_data..];
    }

    // sending full chunks to inner loop
    while data_to_write.len() >= chunk_bytes {
        // allocating new chunk
        let mut chunk: Vec<u8> = Vec::with_capacity(chunk_bytes);
        unsafe { chunk.set_len(chunk_bytes); }
        trace!("PB: chunk length: {}, chunk_bytes: {}", chunk.len(), chunk_bytes);

        chunk.copy_from_slice(&data_to_write[0..chunk_bytes]);
        match rtd.play_tx_dev.as_ref().unwrap().send(chunk) {
            Ok(_) => {}
            Err(err) => {
                error!("{}", err.to_string());
                return Err(Box::new(err));
            }
        }
        data_to_write = &data_to_write[chunk_bytes..];
    }
    if data_to_write.len() > 0 {
        // storing the leftovers
        trace!("PB: leftovers length: {}, data_to_write length: {}", rtd.leftovers.len(), data_to_write.len());
        leftovers_pos = data_to_write.len();
        rtd.leftovers[0..leftovers_pos].copy_from_slice(data_to_write);
    }
    rtd.leftovers_pos.store(leftovers_pos, Ordering::Relaxed);
    Ok(data_len)
}

pub fn do_read(rtd: &mut RuntimeData, input_buffer: &mut [u8], offset: usize, data_len: usize) -> Res<usize> {
    trace!("CAPT: do_read: input_buffer {} bytes, offset {} bytes, reading {} bytes", input_buffer.len(), offset, data_len);
    let chunk_bytes = rtd.chunk_frames * rtd.frame_bytes;
    let mut read_len = 0;
    let mut expected_chunk_nbr = rtd.capt_last_chunk_nbr;
    let buffer = &mut input_buffer[offset..(offset + data_len)];

    // copying leftovers if any to the beginning of the output java_buffer
    let mut leftovers_pos = rtd.leftovers_pos.load(Ordering::Relaxed);
    if leftovers_pos > 0 {
        buffer[0..leftovers_pos].copy_from_slice(&rtd.leftovers[0..leftovers_pos]);
        read_len += leftovers_pos;
        // cleared
        leftovers_pos = 0;
    }

    // reading chunks from the inner loop
    while read_len <= data_len {
        // fully blocking
        match rtd.capt_rx_dev.as_ref().unwrap().recv() {
            Ok((chunk_nbr, data)) => {
                trace!("CAPT: got chunk nbr {}, long {} bytes", chunk_nbr, data.len());
                expected_chunk_nbr += 1;
                if chunk_nbr > expected_chunk_nbr {
                    warn!("CAPT: Samples were dropped, missing {} buffers", chunk_nbr - expected_chunk_nbr);
                    expected_chunk_nbr = chunk_nbr;
                }
                if read_len + chunk_bytes <= data_len {
                    // copying whole chunk to output_buffer
                    buffer[read_len..read_len + chunk_bytes].copy_from_slice(&data[0..chunk_bytes]);
                } else {
                    //copying what fits to output_buffer
                    let available_bytes = data_len - read_len;
                    buffer[read_len..].copy_from_slice(&data[0..available_bytes]);
                    // rest goes to leftovers (both buffer are chunk_bytes long)
                    leftovers_pos = chunk_bytes - available_bytes;
                    rtd.leftovers[0..leftovers_pos].copy_from_slice(&data[available_bytes..]);
                }
                read_len += chunk_bytes;
                // Return the buffer to the queue
                rtd.capt_tx_prealloc.as_ref().unwrap().send(data).unwrap();
            }
            Err(err) => {
                error!("{}", err.to_string());
                return Err(Box::new(err));
            }
        }
    }
    // storing persistent data
    rtd.leftovers_pos.store(leftovers_pos, Ordering::Relaxed);
    rtd.capt_last_chunk_nbr = expected_chunk_nbr;

    // returning only output_buffer, i.e. data_len
    Ok(data_len)
}

pub fn do_get_avail_bytes(rtd: &RuntimeData, dir: &Direction) -> Res<usize> {
    check_direction_from_rt(rtd, &dir, "do_get_avail_bytes")?;
    let avail_bytes = if *dir == Direction::Render {
        // all currently available room without blocking. I.e. remaining room in the queue minus leftover samples
        // (which will be copied to the queue as first)
        let tx = rtd.play_tx_dev.as_ref().unwrap();
        // (tx.capacity().unwrap() + 1 - tx.len()) * rtd.chunk_frames * rtd.frame_bytes
        //     - rtd.leftovers_pos.load(Ordering::Relaxed)
        (tx.capacity().unwrap() - tx.len()) * rtd.chunk_frames * rtd.frame_bytes
    } else {
        // reading without blocking => all currently available samples
        rtd.capt_rx_dev.as_ref().unwrap().len() * rtd.chunk_frames * rtd.frame_bytes
            + rtd.leftovers_pos.load(Ordering::Relaxed)
    };
    trace!("do_get_avail_bytes: {}", avail_bytes);
    Ok(avail_bytes as usize)
}

pub fn do_get_byte_pos(rtd: &RuntimeData, dir: &Direction, java_byte_pos: u64) -> Res<u64> {
    check_direction_from_rt(rtd, &dir, "do_get_byte_pos")?;
    // TODO - reading extra data from audioclient?
    let byte_pos = if *dir == Direction::Render {
        let queued_bytes = rtd.play_tx_dev.as_ref().unwrap().len() * rtd.chunk_frames * rtd.frame_bytes
            + rtd.leftovers_pos.load(Ordering::Relaxed);
        // queued bytes are not played yet, however they are already part of java_byte_pos sent to native - must be subtracted
        java_byte_pos - queued_bytes as u64
    } else {
        let queued_bytes = rtd.capt_rx_dev.as_ref().unwrap().len() * rtd.chunk_frames * rtd.frame_bytes
            + rtd.leftovers_pos.load(Ordering::Relaxed);
        // already in java + what we already have captured in native
        java_byte_pos + queued_bytes as u64
    };
    trace!("do_get_byte_pos: {}", byte_pos);
    Ok(byte_pos as u64)
}

pub fn do_close(rtd: &RuntimeData, dir: &Direction) -> Res<()> {
    check_direction_from_rt(rtd, dir, "do_close")?;
    debug!("requested closing device {}", rtd.device_name);
    rtd.exit_signal.store(true, Ordering::Relaxed);
    Ok(())
}

pub fn do_drain(rtd: &RuntimeData) {
    debug!("draining device {}", rtd.device_name);
    if rtd.dir == Direction::Capture {
        // stopping the capture device first
        rtd.stop_signal.store(true, Ordering::Relaxed);
    }
    loop {
        if rtd.dir == Direction::Render {
            if (rtd.play_tx_dev.as_ref().unwrap().len()) == 0 && rtd.bufferfill_bytes.load(Ordering::Relaxed) == 0 {
                // card has already consumed all samples in the interthread and internal buffers
                rtd.stop_signal.store(true, Ordering::Relaxed);
                break;
            }
        } else {
            if (rtd.capt_rx_dev.as_ref().unwrap().len()) == 0 {
                // java has already consumed all captured data
                break;
            }
        }
        // checking situation every 5 ms
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn do_flush(rtd: &RuntimeData) {
    debug!("flushing device {}", rtd.device_name);
    // consuming all chunks in the interthread buffer
    if rtd.dir == Direction::Render {
        let _ = rtd.play_draining_rx_dev.as_ref().unwrap().try_iter().count();
    } else {
        let _ = rtd.capt_rx_dev.as_ref().unwrap().try_iter().count();
    }
}

fn check_direction(device_dir: &Direction, checked_dir: &Direction, device_id: &str, fn_name: &str) -> Res<()> {
    if device_dir != checked_dir {
        let msg = format!("Called {} for device ID {} with wrong direction {}",
                          fn_name, device_id, checked_dir);
        return Err(msg.into());
    }
    Ok(())
}

fn check_direction_from_rt(rtd: &RuntimeData, dir: &Direction, fn_name: &str) -> Res<()> {
    check_direction(&rtd.dir, dir, &rtd.device_id, fn_name)
}

pub fn device_open(
    device_id: &str,
    dir: &Direction, rate: usize, validbits: usize, frame_bytes: usize,
    channels: usize, dev_period: i64) -> Res<(
    Device,
    AudioClient,
    Handle,
)> {
    let sharemode = ShareMode::Exclusive;
    let (device, dev_name, mut audio_client) = get_device_details(&device_id, &dir)?;

    let wvformats = get_possible_formats(8 * frame_bytes / channels, validbits, rate, channels);
    let wvformat = match find_supported_format(&dev_name, &audio_client, wvformats) {
        Some(ok_wvformat) => {
            debug!("%s: Opening device {}: will use format {:?}", dev_name, ok_wvformat);
            ok_wvformat
        }
        None => {
            let msg = format!("Opening device {}: no supported format found", dev_name);
            return Err(msg.into());
        }
    };
    match audio_client.initialize_client(
        &wvformat,
        dev_period,
        &dir,
        &sharemode,
        false,
    ) {
        Ok(_) => {}
        Err(err) => {
            error!("Calling method audio_client.initialize_client failed: {:?}\n", err);
            return Err(err);
        }
    };
    debug!("initialized {} device {} with device period {} and format {:?}", dir, device_id, dev_period, wvformat);
    let handle = audio_client.set_get_eventhandle()?;
    debug!("Opened Wasapi device {} in {}", dev_name, dir);
    Ok((device, audio_client, handle))
}

fn find_supported_format(dev_name: &str, audio_client: &AudioClient, wvformats: Vec<WaveFormat>) -> Option<WaveFormat> {
    for wvformat in wvformats {
        match get_supported_format(audio_client, dev_name, &wvformat) {
            Some(ok_wvformat) => {
                debug!("Opening device {}: supports format {:?}", dev_name, ok_wvformat);
                return Some(ok_wvformat);
            }
            None => {
                debug!("Opening device {}: unsupported format: {:?}", dev_name, wvformat);
            }
        }
    }
    None
}

fn get_device_details(device_id: &str, dir: &Direction) -> Res<(Device, String, AudioClient)> {
    let (device, dev_dir) = get_device_by_id(&device_id)?;
    check_direction(&dev_dir, &dir, &device_id, "device_open")?;
    let dev_name = device.get_friendlyname()?;
    debug!("Found device {}", dev_name);
    let audio_client = device.get_iaudioclient()?;
    trace!("Got iaudioclient");
    Ok((device, dev_name, audio_client))
}


// Playback loop, play samples received from channel
fn playback_loop(
    audio_client: wasapi::AudioClient,
    handle: wasapi::Handle,
    frame_bytes: usize,
    chunk_frames: usize,
    samplerate: usize,
    sync: PlaySyncData,
) -> Res<()> {
    let tx_cb = sync.tx_cb;
    let mut callbacks = wasapi::EventCallbacks::new();
    callbacks.set_disconnected_callback(move |reason| {
        debug!("PB: Disconnected, reason: {:?}", reason);
        let simplereason = match reason {
            DisconnectReason::FormatChanged => Disconnected::FormatChange,
            _ => Disconnected::Error,
        };
        tx_cb.send(simplereason).unwrap_or(());
    });
    let callbacks_rc = Rc::new(callbacks);
    let callbacks_weak = Rc::downgrade(&callbacks_rc);
    let clock = audio_client.get_audioclock()?;

    let sessioncontrol = audio_client.get_audiosessioncontrol()?;
    sessioncontrol.register_session_notification(callbacks_weak)?;

    // let mut waited_millis = 0;
    // trace!("Waiting for data to start playback, will time out after one second");
    // while sync.rx_play.len() < 2 && waited_millis < 1000 {
    //     thread::sleep(Duration::from_millis(10));
    //     waited_millis += 10;
    // }
    // debug!("Waited for data for {} ms", waited_millis);

    // Raise priority
    let mut task_idx = 0;
    unsafe {
        let _res = AvSetMmThreadCharacteristicsW(PCWSTR::from(&"Pro Audio".into()), &mut task_idx);
    }
    if task_idx > 0 {
        trace!("PB: thread raised priority, task index: {}", task_idx);
    } else {
        warn!("PB: Failed to raise thread priority");
    }

    audio_client.stop_stream()?;
    let mut running = false;
    let mut pos = 0;
    let mut device_prevtime = 0.0;
    let device_freq = clock.get_frequency()? as f64;
    let render_client = audio_client.get_audiorenderclient()?;
    //let file_res: Result<Box<dyn Write>, std::io::Error> = File::create("inner.raw").map(|f| Box::new(f) as Box<dyn Write>);
    //let mut file = file_res.unwrap();
    loop {
        let buffer_free_frames = audio_client.get_available_space_in_frames()?;
        trace!("PB: New buffer frame count {}", buffer_free_frames);
        let device_time = pos as f64 / device_freq;
        //println!("pos {} {}, f {}, time {}, diff {}", pos.0, pos.1, f, devtime, devtime-prevtime);
        //println!("{}",prev_inst.elapsed().as_micros());
        trace!(
            "PB: Device time grew by {} s",
            device_time - device_prevtime
        );
        if buffer_free_frames > 0 && (device_time - device_prevtime) > 1.5 * (buffer_free_frames as f64 / samplerate as f64) as f64 {
            warn!(
                "PB: Missing event! Resetting stream. Interval {} s, expected {} s",
                device_time - device_prevtime,
                buffer_free_frames as f64 / samplerate as f64
            );
            if running {
                audio_client.stop_stream()?;
                audio_client.reset_stream()?;
                audio_client.start_stream()?;
                running = true;
            }
        }
        device_prevtime = device_time;

        if sync.start_signal.load(Ordering::Relaxed) {
            debug!("PB: Starting inner loop");
            if !running {
                audio_client.start_stream()?;
                running = true;
            }
            sync.start_signal.store(false, Ordering::Relaxed);
            // staying in the loop
        }
        if sync.stop_signal.load(Ordering::Relaxed) {
            debug!("PB: Stopping inner loop");
            if running {
                audio_client.stop_stream()?;
                running = false;
            }
            sync.stop_signal.store(false, Ordering::Relaxed);
            // staying in the loop
        }
        if sync.exit_signal.load(Ordering::Relaxed) {
            debug!("PB: Exiting inner loop");
            audio_client.stop_stream()?;
            running = false;
            sync.exit_signal.store(false, Ordering::Relaxed);
            //file.flush();
            return Ok(());
        }


        // reading from data channel with timeout 5ms
        let chunk = match sync.rx_dev.recv_timeout(Duration::from_millis(5)) {
            Ok(chunk) => {
                trace!("PB: got chunk");
                if !running {
                    warn!("PB: received chunk in stopped device, starting automatically!");
                    audio_client.start_stream()?;
                    running = true;
                }
                Some(chunk)
            }
            Err(RecvTimeoutError::Timeout) => {
                trace!("PB: chunk receive timed out, no data");
                // sleeping is provided by recv_timeout(timeout)
                if running {
                    audio_client.stop_stream()?;
                    running = false;
                }
                None
            }
            Err(RecvTimeoutError::Disconnected) => {
                let msg = "PB: data channel is closed";
                error!("{}", msg);
                if running {
                    audio_client.stop_stream()?;
                    running = false;
                }
                return Err(DeviceError::new(msg).into());
            }
        };
        if chunk.is_some() {
            let chunk = chunk.unwrap();
            //let write_res = file.write_all(chunk.as_slice());
            render_client.write_to_device(
                chunk_frames,
                frame_bytes,
                chunk.as_slice(),
                None,
            )?;
            // for reporting position
            sync.wasapi_bufferfill_bytes.store(chunk_frames * frame_bytes, Ordering::Relaxed);
            trace!("PB: write ok");
            let now = Instant::now();
            if handle.wait_for_event(1000).is_err() {
                error!("PB: Error on playback, stopping stream");
                audio_client.stop_stream()?;
                running = false;
                return Err(DeviceError::new("PB: Error on playback").into());
            }
            trace!("PB: waited for event: {:?}", now.elapsed());
            // buffer empty
            sync.wasapi_bufferfill_bytes.store(0, Ordering::Relaxed);
        }
        pos = clock.get_position()?.0;
    }
}

fn capture_loop(
    audio_client: AudioClient,
    handle: Handle,
    frame_bytes: usize,
    chunk_frames: usize,
    samplerate: usize,
    sync: CaptSyncData,
) -> Res<()> {
    let mut chunk_nbr: u64 = 0;

    let mut callbacks = wasapi::EventCallbacks::new();
    callbacks.set_disconnected_callback(move |reason| {
        debug!("CAPT: disconnected, reason: {:?}", reason);
        let simplereason = match reason {
            DisconnectReason::FormatChanged => Disconnected::FormatChange,
            _ => Disconnected::Error,
        };
        sync.tx_cb.send(simplereason).unwrap_or(());
    });

    let callbacks_rc = Rc::new(callbacks);
    let callbacks_weak = Rc::downgrade(&callbacks_rc);
    let mut device_prevtime = None;
    let clock = audio_client.get_audioclock()?;

    let sessioncontrol = audio_client.get_audiosessioncontrol()?;
    sessioncontrol.register_session_notification(callbacks_weak)?;

    audio_client.stop_stream()?;
    let mut running = false;
    let mut inactive = false;

    let mut saved_buffer: Option<Vec<u8>> = None;

    // Raise priority
    let mut task_idx = 0;
    unsafe {
        let _res = AvSetMmThreadCharacteristicsW(PCWSTR::from(&"Pro Audio".into()), &mut task_idx);
    }
    if task_idx > 0 {
        trace!("CAPT: thread raised priority, task index: {}", task_idx);
    } else {
        warn!("CAPT: Failed to raise thread priority");
    }
    let device_freq = clock.get_frequency()? as f64;
    let max_duration = Duration::from_millis(100);
    let sleep_duration = Duration::from_millis(2);

    let capture_client = audio_client.get_audiocaptureclient()?;
    //trace!("Starting capture stream");
    audio_client.stop_stream()?;
    let available_frames = audio_client.get_available_space_in_frames()?;
    trace!("CAPT: Available frames from dev: {}", available_frames);
    if available_frames as usize != chunk_frames {
        error!("CAPT: available_frames {} != chunk_frames {} in EXCLUSIVE mode, failure in wasapi!", available_frames, chunk_frames);
        return Err(DeviceError::new("CAPT: Misbehaving EXCLUSIVE mode").into());
    }

    //trace!("Started capture stream");
    let mut now = Instant::now();
    loop {
        trace!("CAPT: capturing");

        // handling signals
        if sync.start_signal.load(Ordering::Relaxed) {
            debug!("CAPT: Starting device");
            if !running {
                audio_client.start_stream()?;
                running = true;
            }
            sync.start_signal.store(false, Ordering::Relaxed);
            // staying in the loop
        }
        if sync.stop_signal.load(Ordering::Relaxed) {
            debug!("CAPT: Stopping device");
            if running {
                audio_client.stop_stream()?;
                running = false;
            }
            sync.stop_signal.store(false, Ordering::Relaxed);
            // staying in the loop
        }
        if sync.exit_signal.load(Ordering::Relaxed) {
            debug!("CAPT: Exiting inner loop");
            audio_client.stop_stream()?;
            running = false;
            sync.exit_signal.store(false, Ordering::Relaxed);
            return Ok(());
        }

        trace!("CAPT: processed samples for {:?}", now.elapsed());
        now = Instant::now();
        let timeout = 250;
        if handle.wait_for_event(timeout).is_err() {
            trace!("CAPT: Timeout {}ms on event", timeout);
            if !inactive {
                warn!("CAPT: No data received within timeout of {}ms", timeout);
                inactive = true;
            }
            // no data received, continue the loop
            now = Instant::now();
            continue;
        }
        trace!("CAPT: waited for event: {:?}", now.elapsed());
        now = Instant::now();

        // no event timeout, should have received data
        if inactive {
            trace!("CAPT: data received");
            inactive = false;
        }

        // empty buffers are received from the main thread to avoid costly allocation in the inner loop
        let mut data = match saved_buffer {
            Some(buf) => {
                saved_buffer = None;
                buf
            }
            None => {
                trace!("CAPT: Getting preallocated chunk from return queue containing {} items", sync.rx_prealloc.len());
                sync.rx_prealloc.recv().unwrap()
            }
        };

        // adjusting the preallocated buffer to actual length
        // this will proceed only once for each pre-allocated chunk because exclusive mode has fixed buffer size (= available_frames)
        let chunk_bytes = available_frames as usize * frame_bytes;
        if data.len() != chunk_bytes {
            data.resize(chunk_bytes, 0);
        }
        let mut frames_read: u32 = 0;
        let mut flags: BufferFlags = BufferFlags::new(0);
        let mut duration = Duration::from_millis(0);
        while frames_read == 0 {
            (frames_read, flags) = capture_client.read_from_device(frame_bytes as usize, &mut data[0..chunk_bytes])?;
            if frames_read == 0 {
                if duration > max_duration {
                    warn!("CAPT: reading from device took longer than {:?}, aborting", max_duration);
                    break;
                } else {
                    debug!("CAPT: read 0 frames, will try again after sleep {:?}", sleep_duration);
                    sleep(sleep_duration);
                    duration += sleep_duration;
                }
            }
        }
        if frames_read != available_frames {
            warn!("CAPT: expected {} frames, got {} in EXCLUSIVE mode!",available_frames, frames_read);
        }

        if flags.silent {
            debug!("CAPT: buffer marked as silent");
            // zeroing all captured samples
            data.iter_mut().take(chunk_bytes).for_each(|val| *val = 0);
        }

        if flags.data_discontinuity {
            warn!("CAPT: device reported a buffer overrun");
        }
        if flags.timestamp_error {
            warn!("CAPT: device reported a timestamp error");
        }

        trace!("CAPT: Sending chunk to main queue containing {} items", sync.tx_dev.len());
        match sync.tx_dev.try_send((chunk_nbr, data)) {
            Ok(()) => {}
            Err(TrySendError::Full((nbr, data))) => {
                debug!("CAPT: Outer side not consuming chunks, dropping captured chunk {}", nbr);
                saved_buffer = Some(data);
            }
            Err(TrySendError::Disconnected(_)) => {
                error!("CAPT: Error sending , channel from inner thread to main disconnected");
                audio_client.stop_stream()?;
                return Err(DeviceError::new("CAPT: Error sending, channel from inner thread to main disconnected").into());
            }
        }
        chunk_nbr += 1;
        let pos = clock.get_position()?.0;
        let device_time = pos as f64 / device_freq;
        if device_prevtime.is_some() {
            let prevtime = device_prevtime.unwrap();
            //println!("pos {} {}, f {}, time {}, diff {}", pos.0, pos.1, f, devtime, devtime-prevtime);
            //println!("{}",prev_inst.elapsed().as_micros());
            trace!(
            "CAPT: Device time grew by {} s",
            device_time - prevtime
        );
            if available_frames > 0 && (device_time - prevtime) > 1.5 * (available_frames as f64 / samplerate as f64) as f64 {
                warn!(
                "CAPT: Missing event! Interval {} s, expected {} s",
                device_time - prevtime,
                available_frames as f64 / samplerate as f64
            );
                if running {
                    // warn!("CAPT: Resetting stream");
                    // audio_client.stop_stream()?;
                    // audio_client.reset_stream()?;
                    // audio_client.start_stream()?;
                    running = true;
                }
            }
        }
        device_prevtime = Some(device_time);
    }
}