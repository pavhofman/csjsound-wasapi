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
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE};
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
    capt_flushed_cnt: usize,
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

struct DeviceTimeTracker {
    log_prefix: String,
    prev_dev_time: Option<f64>,
    accumulated_frame_time: f64,
}

impl DeviceTimeTracker {
    pub fn new(log_prefix: String) -> DeviceTimeTracker {
        DeviceTimeTracker {
            log_prefix,
            prev_dev_time: None,
            accumulated_frame_time: 0.,
        }
    }

    pub fn reset(&mut self) {
        self.prev_dev_time = None;
        self.accumulated_frame_time = 0.;
    }

    pub fn event_missing(&mut self, dev_time: f64, frame_time: f64) -> bool {
        if dev_time == 0. {
            // invalid value, because we cannot distinguish between S_OK with real dev_time=0 and S_FALSE
            // see S_OK vs. S_FALSE in https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclock-getposition#remarks
            trace!("{}: clock position zero (likely S_FALSE), ignoring", self.log_prefix);
            if self.prev_dev_time.is_some() {
                // dev time is running, need to accumulate matching frame time
                trace!("{}: accumulating frametime for next check", self.log_prefix);
                self.accumulated_frame_time += frame_time;
            }
            // not updating self.prev_dev_time, keeping value from previous check
            return false;
        } else {
            if self.prev_dev_time.is_some() {
                let prev_dev_time = self.prev_dev_time.unwrap();
                let elapsed_dev_time = dev_time - prev_dev_time;
                let elapsed_frame_time = self.accumulated_frame_time + frame_time;
                trace!("{}: Device time grew by {} s", self.log_prefix, elapsed_dev_time);

                // checking for a missed event
                // 1 event time corresponds to 1 frame_time,
                // therefore checking whether elapsed_dev_time is significantly larger than elapsed_frame_time
                if elapsed_frame_time > 0. && elapsed_dev_time > elapsed_frame_time + 0.5 * frame_time {
                    warn!("{}: Missed event: device time grew by {}s, expected {}s",
                    self.log_prefix, elapsed_dev_time, elapsed_frame_time);

                    self.reset();
                    return true;
                }
            }
            // storing dev_time for next check
            self.prev_dev_time = Some(dev_time);
            // since self.prev_dev_time contains current dev_time now (i.e. next check will cover only one event time),
            // accumulated frame_time from previous events must be cleared
            self.accumulated_frame_time = 0.;
            return false;
        }
    }
}


pub fn do_initialize_wasapi() -> Res<()> {
    return match initialize_sta() {
        Ok(_) => {
            Ok(())
        }
        Err(err) => {
            match err.code() {
                // non-fatal results: see https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-coinitializeex#return-value
                S_FALSE => {
                    debug!("Thread already initialized in STA mode");
                    Ok(())
                }
                RPC_E_CHANGED_MODE => {
                    warn!("Thread already initialized in a non-STA mode, continuing");
                    Ok(())
                }
                // fatal errors
                _ => { Err(Box::new(err)) }
            }
        }
    };
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
            debug!("{} device {} supports format {:?}", client.direction, dev_name, *wvformat);
            Some(wvformat.clone())
        }
        Ok(Some(similar_wvfmt)) => {
            // WASAPI specs say this should not happen in exclusive mode
            debug!("{} device {} supports similar format {:?}", client.direction, dev_name, similar_wvfmt);
            Some(similar_wvfmt)
        }
        Err(err) => {
            debug!("{} device {} does not support format {:?}: {}", client.direction, dev_name, wvformat, err);
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
    for (_format, wvformats) in &*WV_FMTS_BY_FORMAT.lock()? {
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

/// lowest common multiple
fn lcm(n1: usize, n2: usize) -> usize {
    let (mut x, mut y) = if n1 > n2 {
        (n1, n2)
    } else {
        (n2, n1)
    };
    let mut rem = x % y;
    while rem != 0 {
        x = y;
        y = rem;
        rem = x % y;
    }
    n1 * n2 / y
}

pub fn do_open_dev(device_id: String, dir: &Direction, rate: usize, validbits: usize, frame_bytes: usize,
                   channels: usize, buffer_bytes: usize) -> Res<RuntimeData> {
    let (_device, device_name, audio_client) = get_device_details(&device_id, dir)?;
    debug!("Opening {} device {}: rate: {}, validbits: {}, frame_bytes: {}, channels: {}, buffer_bytes: {}",
        dir, device_name, rate, validbits, frame_bytes, channels, buffer_bytes);
    let (_def_period_ns00, min_period_ns00) = audio_client.get_periods()?;
    debug!(
        "{}: default period {}, min period {}",
        dir,
        _def_period_ns00, min_period_ns00
    );

    let is_playback = *dir == Direction::Render;

    // period around 30 ms
    let approx_period_ns00 = cmp::max(30 * 10_000, min_period_ns00);
    let align_segment_bytes = if channels <= 16 {
        // can be IntelHDA (max 16 channels by specs) which in addition to frames requires aligning to 128 bytes
        // finding the lowest common multiple
        lcm(frame_bytes, 128)
    } else {
        // only aligning to frames
        frame_bytes
    };
    let align_segment_ns00 = align_segment_bytes as f64 * 10_000_000.0 / rate as f64;

    // aligning
    let align_segments = ((approx_period_ns00 as f64 / align_segment_ns00) + 0.5) as i64;
    debug!("{}: align_segment_bytes: {}, align_segment_ns00: {}, align_segments {} in approx_dev_period {}",
        dir, align_segment_bytes, align_segment_ns00, align_segments, approx_period_ns00);
    let mut period_ns00 = (align_segments as f64 * align_segment_ns00 + 0.5) as i64;
    if period_ns00 < min_period_ns00 {
        // adding one more ns00 segment
        period_ns00 += align_segment_ns00 as i64;
    }
    debug!("{}: Using device period {}", dir, period_ns00);
    // this code assumes device.Initialize will use closely similar buffer to dev_period
    let estimated_chunk_frames = (rate as i64 * period_ns00 / 10_000_000) as usize;
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
        let prealloc_chunks = 2 * chunks;
        // no need to limit the channel size
        let (tx, rx) = unbounded();
        // filling the channel with preallocated chunks
        // estimated_chunk_frames is just an estimate, keeping marging for possibly larger chunk_frames
        let prealloc_chunk_bytes = (1.5 * estimated_chunk_frames as f32 * frame_bytes as f32) as usize;
        debug!("CAPT: Preallocating {} chunks", prealloc_chunks);
        for _ in 0..(prealloc_chunks) {
            let prealloc_chunk = vec![0u8; prealloc_chunk_bytes];
            tx.send(prealloc_chunk)?;
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
            // new thread requires initializing wasapi (STA)
            if let Err(err) = do_initialize_wasapi() {
                let msg = format!("{}: error: {}", &dir_cloned, err);
                tx_state_dev.send(DeviceState::Error(msg)).unwrap_or(());
                return;
            }
            let (_device, audio_client, handle, client_buffer_frames) =
                match device_open(
                    &device_id_cloned,
                    &dir_cloned,
                    rate,
                    validbits,
                    frame_bytes,
                    channels,
                    period_ns00,
                ) {
                    Ok((_device, audio_client, handle)) => {
                        let client_buffer_frames = match audio_client.get_bufferframecount() {
                            Ok(frames) => { frames }
                            Err(err) => {
                                let msg = format!("PB: error: {}", err);
                                tx_state_dev.send(DeviceState::Error(msg)).unwrap_or(());
                                return;
                            }
                        } as usize;
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
        })?;
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
        capt_flushed_cnt: 0,
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
        trace!("PB: new chunk with leftovers and data: length: {}, chunk_bytes: {}", chunk.len(), chunk_bytes);

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
        trace!("PB: new chunk with data only: length: {}, chunk_bytes: {}", chunk.len(), chunk_bytes);

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
        trace!("PB: storing to leftovers: leftovers length: {}, data_to_write length: {}", rtd.leftovers.len(), data_to_write.len());
        leftovers_pos = data_to_write.len();
        rtd.leftovers[0..leftovers_pos].copy_from_slice(data_to_write);
    }
    rtd.leftovers_pos.store(leftovers_pos, Ordering::Relaxed);
    Ok(data_len)
}

pub fn do_read(rtd: &mut RuntimeData, out_buffer: &mut [u8], offset: usize, data_len: usize) -> Res<usize> {
    trace!("CAPT: do_read: input_buffer {} bytes, offset {} bytes, reading {} bytes", out_buffer.len(), offset, data_len);
    let mut read_len = 0;
    let mut expected_chunk_nbr = rtd.capt_last_chunk_nbr;
    let buffer = &mut out_buffer[offset..(offset + data_len)];

    // copying leftovers if any to the beginning of the output java_buffer
    let mut leftovers_pos = rtd.leftovers_pos.load(Ordering::Relaxed);
    if leftovers_pos > 0 {
        // some leftovers available
        if leftovers_pos <= data_len {
            trace!("CAPT: copying all {} leftover bytes to out buffer", leftovers_pos);
            // complete leftovers fit data_len, copying whole leftovers
            buffer[0..leftovers_pos].copy_from_slice(&rtd.leftovers[0..leftovers_pos]);
            read_len += leftovers_pos;
            // cleared
            leftovers_pos = 0;
        } else {
            trace!("CAPT: copying only data_len {} from total {} leftover bytes to out buffer",
                data_len, leftovers_pos);
            // copying only data_len from leftovers
            buffer[0..data_len].copy_from_slice(&rtd.leftovers[0..data_len]);
            // shifting remaining leftovers to start for next do_read
            rtd.leftovers.copy_within(data_len.., 0);
            leftovers_pos = leftovers_pos - data_len;
            trace!("CAPT: kept {} leftover bytes for the next do_read()", leftovers_pos);
            // out buffer is filled up
            read_len = data_len;
        }
    }

    // reading chunks from the inner loop
    while read_len < data_len {
        // fully blocking
        match rtd.capt_rx_dev.as_ref().unwrap().recv() {
            Ok((chunk_nbr, data)) => {
                trace!("CAPT: got chunk nbr {}, long {} bytes", chunk_nbr, data.len());
                // 1 new + flushed in the meantime
                expected_chunk_nbr += 1 + rtd.capt_flushed_cnt as u64;
                // resetting
                rtd.capt_flushed_cnt = 0;
                if chunk_nbr > expected_chunk_nbr {
                    warn!("CAPT: Samples were dropped, missing {} buffers", chunk_nbr - expected_chunk_nbr);
                    expected_chunk_nbr = chunk_nbr;
                }
                let chunk_bytes = data.len();
                let expected_chunk_bytes_for_exclusive = rtd.chunk_frames * rtd.frame_bytes;
                if chunk_bytes != expected_chunk_bytes_for_exclusive {
                    warn!("CAPT: received chunk bytes {} do not correspond to expected chunk bytes {} for EXCLUSIVE access!!",
                        chunk_bytes, expected_chunk_bytes_for_exclusive);
                }

                let available_space_bytes = data_len - read_len;
                if chunk_bytes <= available_space_bytes {
                    trace!("CAPT: copying the whole received chunk of {} bytes to the out buffer, starting from position {} (offset-adjusted)",
                        chunk_bytes, read_len);
                    buffer[read_len..(read_len + chunk_bytes)].copy_from_slice(&data[0..chunk_bytes]);
                    read_len += chunk_bytes;
                } else {
                    //copying whatever fits to output_buffer
                    trace!("CAPT: copying only {} bytes of the received chunk to fill up the out buffer, starting from position {} (offset-adjusted)",
                        available_space_bytes, read_len);
                    buffer[read_len..].copy_from_slice(&data[0..available_space_bytes]);
                    // out buffer is filled up
                    read_len = data_len;
                    // the rest goes to leftovers
                    leftovers_pos = chunk_bytes - available_space_bytes;
                    trace!("CAPT: copying the remaining {} bytes of the received chunk to leftovers", leftovers_pos);
                    rtd.leftovers[0..leftovers_pos].copy_from_slice(&data[available_space_bytes..]);
                }

                // Return the received buffer to the queue
                rtd.capt_tx_prealloc.as_ref().unwrap().send(data)?;
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
        sleep(Duration::from_millis(5));
    }
}

pub fn do_flush(rtd: &mut RuntimeData) -> Res<()> {
    debug!("flushing device {}", rtd.device_name);
    // consuming all chunks in the interthread buffer
    let cnt = if rtd.dir == Direction::Render {
        rtd.play_draining_rx_dev.as_ref().unwrap().try_iter().count()
    } else {
        let mut cnt = 0;
        // received buffers must be returned to the prealloc channel
        for (_chunk_nbr, data) in rtd.capt_rx_dev.as_ref().unwrap().try_iter() {
            rtd.capt_tx_prealloc.as_ref().unwrap().send(data)?;
            cnt += 1;
        }
        rtd.capt_flushed_cnt += cnt;
        cnt
    };
    trace!("flushed {} chunks from device {}", cnt, rtd.device_name);
    Ok(())
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

    let wvformats = get_possible_formats(8 * frame_bytes / channels, validbits, rate, channels)?;
    let wvformat = match find_supported_format(&dev_name, &audio_client, wvformats) {
        Some(ok_wvformat) => {
            debug!("Opening {} device {}: will use format {:?}", dir, dev_name, ok_wvformat);
            ok_wvformat
        }
        None => {
            let msg = format!("Opening {} device {}: no supported format found", dir, dev_name);
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
    audio_client: AudioClient,
    handle: Handle,
    frame_bytes: usize,
    chunk_frames: usize,
    samplerate: usize,
    sync: PlaySyncData,
) -> Res<()> {
    let tx_cb = sync.tx_cb;
    let mut callbacks = wasapi::EventCallbacks::new();
    callbacks.set_disconnected_callback(move |reason| {
        debug!("PB INNER: Disconnected, reason: {:?}", reason);
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
        trace!("PB INNER: thread raised priority, task index: {}", task_idx);
    } else {
        warn!("PB INNER: Failed to raise thread priority");
    }

    audio_client.stop_stream()?;
    let mut running = false;
    let mut time_tracker = DeviceTimeTracker::new("PB INNER".into());
    let device_freq = clock.get_frequency()? as f64;
    let render_client = audio_client.get_audiorenderclient()?;
    //let file_res: Result<Box<dyn Write>, std::io::Error> = File::create("inner.raw").map(|f| Box::new(f) as Box<dyn Write>);
    //let mut file = file_res.unwrap();
    let mut now = Instant::now();
    loop {
        let buffer_free_frames = audio_client.get_available_space_in_frames()?;
        trace!("PB INNER: New buffer frame count {}", buffer_free_frames);

        if sync.start_signal.load(Ordering::Relaxed) {
            debug!("PB INNER: Starting inner loop, {}", if running {"stream is already running"} else {"starting stream"});
            if !running {
                audio_client.start_stream()?;
                running = true;
                time_tracker.reset();
            }
            sync.start_signal.store(false, Ordering::Relaxed);
            // staying in the loop
        }
        if sync.stop_signal.load(Ordering::Relaxed) {
            debug!("PB INNER: Stopping inner loop");
            if running {
                audio_client.stop_stream()?;
                running = false;
                time_tracker.reset();
            }
            sync.stop_signal.store(false, Ordering::Relaxed);
            // staying in the loop
        }
        if sync.exit_signal.load(Ordering::Relaxed) {
            debug!("PB INNER: Exiting inner loop");
            audio_client.stop_stream()?;
            sync.exit_signal.store(false, Ordering::Relaxed);
            //file.flush();
            return Ok(());
        }


        // reading from data channel with timeout 5ms
        let chunk = match sync.rx_dev.recv_timeout(Duration::from_millis(5)) {
            Ok(chunk) => {
                trace!("PB INNER: got chunk");
                if !running {
                    warn!("PB INNER: received chunk in stopped device, starting automatically!");
                    audio_client.start_stream()?;
                    running = true;
                    time_tracker.reset();
                }
                Some(chunk)
            }
            Err(RecvTimeoutError::Timeout) => {
                trace!("PB INNER: chunk receive timed out, no data");
                // sleeping is provided by recv_timeout(timeout)
                if running {
                    audio_client.stop_stream()?;
                    running = false;
                    time_tracker.reset();
                }
                None
            }
            Err(RecvTimeoutError::Disconnected) => {
                // while inner was waiting, the outer loop could have been closed
                return if sync.exit_signal.load(Ordering::Relaxed) {
                    debug!("PB INNER: Exiting inner loop");
                    audio_client.stop_stream()?;
                    sync.exit_signal.store(false, Ordering::Relaxed);
                    //file.flush();
                    Ok(())
                } else {
                    let msg = "PB INNER: data channel is closed although no exit was requested";
                    error!("{}", msg);
                    if running {
                        audio_client.stop_stream()?;
                    }
                    Err(DeviceError::new(msg).into())
                };
            }
        };
        trace!("PB INNER: loop spent outside of wait_for_event {:?}", now.elapsed());
        now = Instant::now();
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
            trace!("PB INNER: write ok, loop spent writing data to device {:?}", now.elapsed());
            now = Instant::now();
            if handle.wait_for_event(1000).is_err() {
                error!("PB INNER: Error on playback, stopping stream");
                audio_client.stop_stream()?;
                return Err(DeviceError::new("PB INNER: Error on playback").into());
            }
            trace!("PB INNER: loop spent in wait_for_event {:?}", now.elapsed());
            now = Instant::now();
            // buffer empty
            sync.wasapi_bufferfill_bytes.store(0, Ordering::Relaxed);
        }
        let pos = clock.get_position()?.0;
        let device_time = pos as f64 / device_freq;
        if time_tracker.event_missing(device_time, buffer_free_frames as f64 / samplerate as f64) {
            warn!("PB INNER: Missed event");
            if running {
                warn!("PB INNER: resetting stream");
                audio_client.stop_stream()?;
                audio_client.reset_stream()?;
                audio_client.start_stream()?;
                time_tracker.reset();
            }
        }
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
        debug!("CAPT INNER: disconnected, reason: {:?}", reason);
        let simplereason = match reason {
            DisconnectReason::FormatChanged => Disconnected::FormatChange,
            _ => Disconnected::Error,
        };
        sync.tx_cb.send(simplereason).unwrap_or(());
    });

    let callbacks_rc = Rc::new(callbacks);
    let callbacks_weak = Rc::downgrade(&callbacks_rc);
    let mut time_tracker = DeviceTimeTracker::new("CAPT INNER".into());
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
        trace!("CAPT INNER: thread raised priority, task index: {}", task_idx);
    } else {
        warn!("CAPT INNER: Failed to raise thread priority");
    }
    let device_freq = clock.get_frequency()? as f64;
    let max_duration = Duration::from_millis(100);
    let sleep_duration = Duration::from_millis(2);

    let capture_client = audio_client.get_audiocaptureclient()?;
    //trace!("Starting capture stream");
    audio_client.stop_stream()?;
    let available_frames = audio_client.get_available_space_in_frames()?;
    trace!("CAPT INNER: Available frames from dev: {}", available_frames);
    if available_frames as usize != chunk_frames {
        error!("CAPT INNER: available_frames {} != chunk_frames {} in EXCLUSIVE mode, failure in wasapi!", available_frames, chunk_frames);
        return Err(DeviceError::new("CAPT INNER: Misbehaving EXCLUSIVE mode").into());
    }

    //trace!("Started capture stream");
    let mut now = Instant::now();
    loop {
        trace!("CAPT INNER: capturing");

        // handling signals
        if sync.start_signal.load(Ordering::Relaxed) {
            debug!("CAPT INNER: Starting device");
            if !running {
                audio_client.start_stream()?;
                running = true;
                time_tracker.reset();
            }
            sync.start_signal.store(false, Ordering::Relaxed);
            // staying in the loop
        }
        if sync.stop_signal.load(Ordering::Relaxed) {
            debug!("CAPT INNER: Stopping device");
            if running {
                audio_client.stop_stream()?;
                running = false;
                time_tracker.reset();
            }
            sync.stop_signal.store(false, Ordering::Relaxed);
            // staying in the loop with running=false
            continue;
        }
        if sync.exit_signal.load(Ordering::Relaxed) {
            debug!("CAPT INNER: Exiting inner loop");
            audio_client.stop_stream()?;
            sync.exit_signal.store(false, Ordering::Relaxed);
            return Ok(());
        }

        if !running {
            // Stopped but not exiting: must stay in the capture loop but cannot read from the device.
            // Shortly wait to avoid CPU hogging and continue looping
            sleep(Duration::from_millis(2));
            continue;
        }

        trace!("CAPT INNER: loop spent outside of wait_for_event {:?}", now.elapsed());
        now = Instant::now();
        let timeout = 250;
        if handle.wait_for_event(timeout).is_err() {
            trace!("CAPT INNER: Timeout {}ms on event", timeout);
            if !inactive {
                warn!("CAPT INNER: No data received within timeout of {}ms, inactive", timeout);
                inactive = true;
            }
            // no data received, continue the loop
            now = Instant::now();
            continue;
        }
        trace!("CAPT INNER: loop spent in wait_for_event {:?}", now.elapsed());
        now = Instant::now();

        // no event timeout, should have received data
        if inactive {
            trace!("CAPT INNER: resuming, data received");
            inactive = false;
        }

        // while waiting for event (the largest wait in the loop), stop/exit signals could have arrived. Must check again
        if sync.stop_signal.load(Ordering::Relaxed) {
            debug!("CAPT INNER: Stopping device");
            if running {
                audio_client.stop_stream()?;
                running = false;
                time_tracker.reset();
            }
            sync.stop_signal.store(false, Ordering::Relaxed);
            // staying in the loop with running=false
            continue;
        }
        if sync.exit_signal.load(Ordering::Relaxed) {
            debug!("CAPT INNER: Exiting inner loop");
            audio_client.stop_stream()?;
            sync.exit_signal.store(false, Ordering::Relaxed);
            return Ok(());
        }

        // empty buffers are received from the main thread to avoid costly allocation in the inner loop
        let mut data = match saved_buffer {
            Some(buf) => {
                saved_buffer = None;
                buf
            }
            None => {
                trace!("CAPT INNER: Getting preallocated chunk from return queue containing {} items", sync.rx_prealloc.len());
                match sync.rx_prealloc.recv() {
                    Ok(buf) => { buf }
                    Err(err) => {
                        // If rx_prealloc was for some reason waiting for returned chunks and the device was closed in the mean time,
                        // RecvError would be thrown.
                        // Checking for exit signal to ignore this error condition.
                        return if sync.exit_signal.load(Ordering::Relaxed) {
                            debug!("CAPT INNER: Exiting inner loop");
                            audio_client.stop_stream()?;
                            sync.exit_signal.store(false, Ordering::Relaxed);
                            Ok(())
                        } else {
                            // real error
                            error!("{}", err.to_string());
                            Err(Box::new(err))
                        };
                    }
                }
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
                    warn!("CAPT INNER: reading from device took longer than {:?}, aborting", max_duration);
                    break;
                } else {
                    debug!("CAPT INNER: read 0 frames, will try again after sleep {:?}", sleep_duration);
                    sleep(sleep_duration);
                    duration += sleep_duration;
                }
            }
        }
        if frames_read != available_frames {
            warn!("CAPT INNER: expected {} frames, got {} in EXCLUSIVE mode!",available_frames, frames_read);
        }

        if flags.silent {
            debug!("CAPT INNER: buffer marked as silent");
            // zeroing all captured samples
            data.iter_mut().take(chunk_bytes).for_each(|val| *val = 0);
        }

        if flags.data_discontinuity {
            warn!("CAPT INNER: device reported a buffer overrun");
        }
        if flags.timestamp_error {
            warn!("CAPT INNER: device reported a timestamp error");
        }

        trace!("CAPT INNER: Sending a new chunk nbr. {} to main queue which contains {} unconsumed chunks", chunk_nbr, sync.tx_dev.len());
        match sync.tx_dev.try_send((chunk_nbr, data)) {
            Ok(()) => {
                trace!("CAPT INNER: Chunk nbr. {} sent OK", chunk_nbr);
            }
            Err(TrySendError::Full((nbr, data))) => {
                debug!("CAPT INNER: Outer side not consuming chunks, dropping the captured chunk {}", nbr);
                saved_buffer = Some(data);
            }
            Err(TrySendError::Disconnected(_)) => {
                error!("CAPT INNER: Error sending , channel from inner thread to main disconnected");
                audio_client.stop_stream()?;
                return Err(DeviceError::new("CAPT INNER: Error sending, channel from inner thread to main disconnected").into());
            }
        }
        chunk_nbr += 1;
        let pos = clock.get_position()?.0;
        let device_time = pos as f64 / device_freq;
        if time_tracker.event_missing(device_time, available_frames as f64 / samplerate as f64) {
            warn!("CAPT INNER: Missed event");
            // if running {
            //     warn!("CAPT INNER: resetting stream");
            //     audio_client.stop_stream()?;
            //     audio_client.reset_stream()?;
            //     audio_client.start_stream()?;
            //     time_tracker.reset();
            // }
        }
    }
}