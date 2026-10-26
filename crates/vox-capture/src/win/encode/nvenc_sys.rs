//! Minimal hand-written NVENC API bindings (only what the encoder uses).
//!
//! Source: FFmpeg `nv-codec-headers` tag **n13.0.19.1** (`include/ffnvcodec/nvEncodeAPI.h`, MIT),
//! NVENC API **13.0** (driver 570+). Sizes and field offsets were checked against MSVC
//! (`sizeof`/`offsetof` on x64) and are pinned by the `const` asserts at the bottom; a mismatch
//! fails the build instead of corrupting memory at runtime.
//!
//! Bitfields are modelled as plain `u32` words with the bit constants below.

#![allow(non_camel_case_types, non_snake_case, dead_code, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type GUID = windows_core::GUID;
pub type NVENCSTATUS = i32;

pub const NVENCAPI_MAJOR_VERSION: u32 = 13;
pub const NVENCAPI_MINOR_VERSION: u32 = 0;
pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);

const fn struct_version(v: u32) -> u32 {
    NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
}

pub const NV_ENC_CAPS_PARAM_VER: u32 = struct_version(1);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_RC_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENC_CONFIG_VER: u32 = struct_version(9) | (1 << 31);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(2) | (1 << 31);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(5) | (1 << 31);
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2) | (1 << 31);
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(5);
pub const NV_ENC_EVENT_PARAMS_VER: u32 = struct_version(2);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);

pub const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;

// GUIDs.
pub const NV_ENC_CODEC_H264_GUID: GUID = GUID::from_u128(0x6bc82762_4e63_4ca4_aa85_1e50f321f6bf);
pub const NV_ENC_H264_PROFILE_MAIN_GUID: GUID =
    GUID::from_u128(0x60b5c1d4_67fe_4790_94d5_c4726d7b6e6d);
pub const NV_ENC_PRESET_P1_GUID: GUID = GUID::from_u128(0xfc0a8d3e_45f8_4cf8_80c7_298871590ebf);
pub const NV_ENC_PRESET_P2_GUID: GUID = GUID::from_u128(0xf581cfb8_88d6_4381_93f0_df13f9c27dab);
pub const NV_ENC_PRESET_P3_GUID: GUID = GUID::from_u128(0x36850110_3a07_441f_94d5_3670631f91f6);
pub const NV_ENC_PRESET_P4_GUID: GUID = GUID::from_u128(0x90a7b826_df06_4862_b9d2_cd6d73a08681);
pub const NV_ENC_PRESET_P5_GUID: GUID = GUID::from_u128(0x21c6e6b4_297a_4cba_998f_b6cbde72ade3);
pub const NV_ENC_PRESET_P6_GUID: GUID = GUID::from_u128(0x8e75c279_6299_4ab6_8302_0b215a335cf5);
pub const NV_ENC_PRESET_P7_GUID: GUID = GUID::from_u128(0x84848c12_6f71_4c13_931b_53e283f57974);

/// Preset GUID for P1..=P7 (clamped).
pub fn preset_guid(p: u8) -> GUID {
    match p {
        0 | 1 => NV_ENC_PRESET_P1_GUID,
        2 => NV_ENC_PRESET_P2_GUID,
        3 => NV_ENC_PRESET_P3_GUID,
        4 => NV_ENC_PRESET_P4_GUID,
        5 => NV_ENC_PRESET_P5_GUID,
        6 => NV_ENC_PRESET_P6_GUID,
        _ => NV_ENC_PRESET_P7_GUID,
    }
}

// Enums (C enums are 32-bit).
pub const NV_ENC_SUCCESS: NVENCSTATUS = 0;
pub const NV_ENC_ERR_LOCK_BUSY: NVENCSTATUS = 13;
pub const NV_ENC_ERR_NEED_MORE_INPUT: NVENCSTATUS = 17;

pub const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0;
pub const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0;
pub const NV_ENC_INPUT_IMAGE: u32 = 0;
pub const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x0000_0001;
pub const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;
pub const NV_ENC_PARAMS_RC_VBR: u32 = 1;
pub const NV_ENC_PARAMS_RC_CBR: u32 = 2;
pub const NV_ENC_MULTI_PASS_DISABLED: u32 = 0;
pub const NV_ENC_TWO_PASS_QUARTER_RESOLUTION: u32 = 1;
pub const NV_ENC_TWO_PASS_FULL_RESOLUTION: u32 = 2;
pub const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 1;
pub const NV_ENC_PIC_TYPE_IDR: u32 = 3;
pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
pub const NV_ENC_PIC_FLAG_EOS: u32 = 0x8;

pub const NV_ENC_CAPS_WIDTH_MAX: u32 = 16;
pub const NV_ENC_CAPS_HEIGHT_MAX: u32 = 17;
pub const NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE: u32 = 20;
pub const NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT: u32 = 30;

pub const NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
pub const NV_ENC_VUI_COLOR_PRIMARIES_BT709: u32 = 1;
pub const NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709: u32 = 1;
pub const NV_ENC_VUI_MATRIX_COEFFS_BT709: u32 = 1;

// Bitfield bits.
pub const H264_OUTPUT_AUD: u32 = 1 << 6;
pub const H264_ENABLE_INTRA_REFRESH: u32 = 1 << 10;
pub const H264_REPEAT_SPSPPS: u32 = 1 << 12;
pub const H264_ENABLE_LTR: u32 = 1 << 14;
pub const RC_ENABLE_MIN_QP: u32 = 1 << 0;
pub const RC_ENABLE_MAX_QP: u32 = 1 << 1;
pub const RC_ENABLE_AQ: u32 = 1 << 3;
pub const RC_ENABLE_LOOKAHEAD: u32 = 1 << 5;
pub const RC_ENABLE_TEMPORAL_AQ: u32 = 1 << 8;
pub const RC_ZERO_REORDER_DELAY: u32 = 1 << 9;
pub const RC_AQ_STRENGTH_SHIFT: u32 = 12;
pub const RC_AQ_STRENGTH_MASK: u32 = 0xf << 12;
pub const RECONF_RESET_ENCODER: u32 = 1 << 0;
pub const RECONF_FORCE_IDR: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_QP {
    pub qpInterP: u32,
    pub qpInterB: u32,
    pub qpIntra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CAPS_PARAM {
    pub version: u32,
    pub capsToQuery: u32,
    pub reserved: [u32; 62],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_RC_PARAMS {
    pub version: u32,
    pub rateControlMode: u32,
    pub constQP: NV_ENC_QP,
    pub averageBitRate: u32,
    pub maxBitRate: u32,
    pub vbvBufferSize: u32,
    pub vbvInitialDelay: u32,
    /// enableMinQP:1 enableMaxQP:1 enableInitialRCQP:1 enableAQ:1 reserved:1 enableLookahead:1
    /// disableIadapt:1 disableBadapt:1 enableTemporalAQ:1 zeroReorderDelay:1 enableNonRefP:1
    /// strictGOPTarget:1 aqStrength:4 enableExtLookahead:1 reserved:15
    pub flags: u32,
    pub minQP: NV_ENC_QP,
    pub maxQP: NV_ENC_QP,
    pub initialRCQP: NV_ENC_QP,
    pub temporallayerIdxMask: u32,
    pub temporalLayerQP: [u8; 8],
    pub targetQuality: u8,
    pub targetQualityLSB: u8,
    pub lookaheadDepth: u16,
    pub lowDelayKeyFrameScale: u8,
    pub yDcQPIndexOffset: i8,
    pub uDcQPIndexOffset: i8,
    pub vDcQPIndexOffset: i8,
    pub qpMapMode: u32,
    pub multiPass: u32,
    pub alphaLayerBitrateRatio: u32,
    pub cbQPIndexOffset: i8,
    pub crQPIndexOffset: i8,
    pub reserved2: u16,
    pub lookaheadLevel: u32,
    pub viewBitrateRatios: [u8; 7],
    pub reserved3: u8,
    pub reserved1: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_H264_VUI_PARAMETERS {
    pub overscanInfoPresentFlag: u32,
    pub overscanInfo: u32,
    pub videoSignalTypePresentFlag: u32,
    pub videoFormat: u32,
    pub videoFullRangeFlag: u32,
    pub colourDescriptionPresentFlag: u32,
    pub colourPrimaries: u32,
    pub transferCharacteristics: u32,
    pub colourMatrix: u32,
    pub chromaSampleLocationFlag: u32,
    pub chromaSampleLocationTop: u32,
    pub chromaSampleLocationBot: u32,
    pub bitstreamRestrictionFlag: u32,
    pub timingInfoPresentFlag: u32,
    pub numUnitInTicks: u32,
    pub timeScale: u32,
    pub reserved: [u32; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_H264 {
    /// enableTemporalSVC .. enableTimeCode bitfield word (see `H264_*`).
    pub flags: u32,
    pub level: u32,
    pub idrPeriod: u32,
    pub separateColourPlaneFlag: u32,
    pub disableDeblockingFilterIDC: u32,
    pub numTemporalLayers: u32,
    pub spsId: u32,
    pub ppsId: u32,
    pub adaptiveTransformMode: u32,
    pub fmoMode: u32,
    pub bdirectMode: u32,
    pub entropyCodingMode: u32,
    pub stereoMode: u32,
    pub intraRefreshPeriod: u32,
    pub intraRefreshCnt: u32,
    pub maxNumRefFrames: u32,
    pub sliceMode: u32,
    pub sliceModeData: u32,
    pub h264VUIParameters: NV_ENC_CONFIG_H264_VUI_PARAMETERS,
    pub ltrNumFrames: u32,
    pub ltrTrustMode: u32,
    pub chromaFormatIDC: u32,
    pub maxTemporalLayers: u32,
    pub useBFramesAsRef: u32,
    pub numRefL0: u32,
    pub numRefL1: u32,
    pub outputBitDepth: u32,
    pub inputBitDepth: u32,
    pub tfLevel: u32,
    pub reserved1: [u32; 264],
    pub reserved2: [*mut c_void; 64],
}

/// `NV_ENC_CODEC_CONFIG` union; only the H.264 member is used. Its size equals the H.264 member.
#[repr(C)]
#[derive(Clone, Copy)]
pub union NV_ENC_CODEC_CONFIG {
    pub h264Config: NV_ENC_CONFIG_H264,
    pub reserved: [u32; 320],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG {
    pub version: u32,
    pub profileGUID: GUID,
    pub gopLength: u32,
    pub frameIntervalP: i32,
    pub monoChromeEncoding: u32,
    pub frameFieldMode: u32,
    pub mvPrecision: u32,
    pub rcParams: NV_ENC_RC_PARAMS,
    pub encodeCodecConfig: NV_ENC_CODEC_CONFIG,
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE {
    pub counts: u32,
    pub reserved1: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_INITIALIZE_PARAMS {
    pub version: u32,
    pub encodeGUID: GUID,
    pub presetGUID: GUID,
    pub encodeWidth: u32,
    pub encodeHeight: u32,
    pub darWidth: u32,
    pub darHeight: u32,
    pub frameRateNum: u32,
    pub frameRateDen: u32,
    pub enableEncodeAsync: u32,
    pub enablePTD: u32,
    /// reportSliceOffsets .. enableUniDirectionalB bitfield word.
    pub flags: u32,
    pub privDataSize: u32,
    pub reserved: u32,
    pub privData: *mut c_void,
    pub encodeConfig: *mut NV_ENC_CONFIG,
    pub maxEncodeWidth: u32,
    pub maxEncodeHeight: u32,
    pub maxMEHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
    pub tuningInfo: u32,
    pub bufferFormat: u32,
    pub numStateBuffers: u32,
    pub outputStatsLevel: u32,
    pub reserved1: [u32; 284],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_RECONFIGURE_PARAMS {
    pub version: u32,
    pub reserved: u32,
    pub reInitEncodeParams: NV_ENC_INITIALIZE_PARAMS,
    /// resetEncoder:1 forceIDR:1 reserved:30
    pub flags: u32,
    pub reserved2: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_PRESET_CONFIG {
    pub version: u32,
    pub reserved: u32,
    pub presetCfg: NV_ENC_CONFIG,
    pub reserved1: [u32; 256],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_PIC_PARAMS {
    pub version: u32,
    pub inputWidth: u32,
    pub inputHeight: u32,
    pub inputPitch: u32,
    pub encodePicFlags: u32,
    pub frameIdx: u32,
    pub inputTimeStamp: u64,
    pub inputDuration: u64,
    pub inputBuffer: *mut c_void,
    pub outputBitstream: *mut c_void,
    pub completionEvent: *mut c_void,
    pub bufferFmt: u32,
    pub pictureStruct: u32,
    pub pictureType: u32,
    /// `NV_ENC_CODEC_PIC_PARAMS` union (unused: all-zero means encoder defaults).
    pub codecPicParams: [u64; 193],
    pub meHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
    pub meExternalHints: *mut c_void,
    pub reserved2: [u32; 7],
    pub reserved5: [*mut c_void; 2],
    pub qpDeltaMap: *mut i8,
    pub qpDeltaMapSize: u32,
    pub reservedBitFields: u32,
    pub meHintRefPicDist: [u16; 2],
    pub reserved4: u32,
    pub alphaBuffer: *mut c_void,
    pub meExternalSbHints: *mut c_void,
    pub meSbHintsCount: u32,
    pub stateBufferIdx: u32,
    pub outputReconBuffer: *mut c_void,
    pub reserved3: [u32; 284],
    pub reserved6: [*mut c_void; 57],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_LOCK_BITSTREAM {
    pub version: u32,
    /// doNotWait:1 ltrFrame:1 getRCStats:1 reserved:29
    pub flags: u32,
    pub outputBitstream: *mut c_void,
    pub sliceOffsets: *mut u32,
    pub frameIdx: u32,
    pub hwEncodeStatus: u32,
    pub numSlices: u32,
    pub bitstreamSizeInBytes: u32,
    pub outputTimeStamp: u64,
    pub outputDuration: u64,
    pub bitstreamBufferPtr: *mut c_void,
    pub pictureType: u32,
    pub pictureStruct: u32,
    pub frameAvgQP: u32,
    pub frameSatd: u32,
    pub ltrFrameIdx: u32,
    pub ltrFrameBitmap: u32,
    pub temporalId: u32,
    pub intraMBCount: u32,
    pub interMBCount: u32,
    pub averageMVX: i32,
    pub averageMVY: i32,
    pub alphaLayerSizeInBytes: u32,
    pub outputStatsPtrSize: u32,
    pub reserved: u32,
    pub outputStatsPtr: *mut c_void,
    pub frameIdxDisplay: u32,
    pub reserved1: [u32; 219],
    pub reserved2: [*mut c_void; 63],
    pub reservedInternal: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_MAP_INPUT_RESOURCE {
    pub version: u32,
    pub subResourceIndex: u32,
    pub inputResource: *mut c_void,
    pub registeredResource: *mut c_void,
    pub mappedResource: *mut c_void,
    pub mappedBufferFmt: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_REGISTER_RESOURCE {
    pub version: u32,
    pub resourceType: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub subResourceIndex: u32,
    pub resourceToRegister: *mut c_void,
    pub registeredResource: *mut c_void,
    pub bufferFormat: u32,
    pub bufferUsage: u32,
    pub pInputFencePoint: *mut c_void,
    pub chromaOffset: [u32; 2],
    pub chromaOffsetIn: [u32; 2],
    pub reserved1: [u32; 244],
    pub reserved2: [*mut c_void; 61],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CREATE_BITSTREAM_BUFFER {
    pub version: u32,
    pub size: u32,
    pub memoryHeap: u32,
    pub reserved: u32,
    pub bitstreamBuffer: *mut c_void,
    pub bitstreamBufferPtr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_EVENT_PARAMS {
    pub version: u32,
    pub reserved: u32,
    pub completionEvent: *mut c_void,
    pub reserved1: [u32; 254],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    pub version: u32,
    pub deviceType: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub apiVersion: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

type Enc = *mut c_void;
type Unused = Option<unsafe extern "system" fn()>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENCODE_API_FUNCTION_LIST {
    pub version: u32,
    pub reserved: u32,
    pub nvEncOpenEncodeSession: Unused,
    pub nvEncGetEncodeGUIDCount:
        Option<unsafe extern "system" fn(Enc, *mut u32) -> NVENCSTATUS>,
    pub nvEncGetEncodeProfileGUIDCount: Unused,
    pub nvEncGetEncodeProfileGUIDs: Unused,
    pub nvEncGetEncodeGUIDs:
        Option<unsafe extern "system" fn(Enc, *mut GUID, u32, *mut u32) -> NVENCSTATUS>,
    pub nvEncGetInputFormatCount: Unused,
    pub nvEncGetInputFormats: Unused,
    pub nvEncGetEncodeCaps: Option<
        unsafe extern "system" fn(Enc, GUID, *mut NV_ENC_CAPS_PARAM, *mut i32) -> NVENCSTATUS,
    >,
    pub nvEncGetEncodePresetCount: Unused,
    pub nvEncGetEncodePresetGUIDs: Unused,
    pub nvEncGetEncodePresetConfig: Unused,
    pub nvEncInitializeEncoder:
        Option<unsafe extern "system" fn(Enc, *mut NV_ENC_INITIALIZE_PARAMS) -> NVENCSTATUS>,
    pub nvEncCreateInputBuffer: Unused,
    pub nvEncDestroyInputBuffer: Unused,
    pub nvEncCreateBitstreamBuffer: Option<
        unsafe extern "system" fn(Enc, *mut NV_ENC_CREATE_BITSTREAM_BUFFER) -> NVENCSTATUS,
    >,
    pub nvEncDestroyBitstreamBuffer:
        Option<unsafe extern "system" fn(Enc, *mut c_void) -> NVENCSTATUS>,
    pub nvEncEncodePicture:
        Option<unsafe extern "system" fn(Enc, *mut NV_ENC_PIC_PARAMS) -> NVENCSTATUS>,
    pub nvEncLockBitstream:
        Option<unsafe extern "system" fn(Enc, *mut NV_ENC_LOCK_BITSTREAM) -> NVENCSTATUS>,
    pub nvEncUnlockBitstream: Option<unsafe extern "system" fn(Enc, *mut c_void) -> NVENCSTATUS>,
    pub nvEncLockInputBuffer: Unused,
    pub nvEncUnlockInputBuffer: Unused,
    pub nvEncGetEncodeStats: Unused,
    pub nvEncGetSequenceParams: Unused,
    pub nvEncRegisterAsyncEvent:
        Option<unsafe extern "system" fn(Enc, *mut NV_ENC_EVENT_PARAMS) -> NVENCSTATUS>,
    pub nvEncUnregisterAsyncEvent:
        Option<unsafe extern "system" fn(Enc, *mut NV_ENC_EVENT_PARAMS) -> NVENCSTATUS>,
    pub nvEncMapInputResource:
        Option<unsafe extern "system" fn(Enc, *mut NV_ENC_MAP_INPUT_RESOURCE) -> NVENCSTATUS>,
    pub nvEncUnmapInputResource:
        Option<unsafe extern "system" fn(Enc, *mut c_void) -> NVENCSTATUS>,
    pub nvEncDestroyEncoder: Option<unsafe extern "system" fn(Enc) -> NVENCSTATUS>,
    pub nvEncInvalidateRefFrames: Unused,
    pub nvEncOpenEncodeSessionEx: Option<
        unsafe extern "system" fn(*mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, *mut Enc) -> NVENCSTATUS,
    >,
    pub nvEncRegisterResource:
        Option<unsafe extern "system" fn(Enc, *mut NV_ENC_REGISTER_RESOURCE) -> NVENCSTATUS>,
    pub nvEncUnregisterResource:
        Option<unsafe extern "system" fn(Enc, *mut c_void) -> NVENCSTATUS>,
    pub nvEncReconfigureEncoder:
        Option<unsafe extern "system" fn(Enc, *mut NV_ENC_RECONFIGURE_PARAMS) -> NVENCSTATUS>,
    pub reserved1: *mut c_void,
    pub nvEncCreateMVBuffer: Unused,
    pub nvEncDestroyMVBuffer: Unused,
    pub nvEncRunMotionEstimationOnly: Unused,
    pub nvEncGetLastErrorString:
        Option<unsafe extern "system" fn(Enc) -> *const std::ffi::c_char>,
    pub nvEncSetIOCudaStreams: Unused,
    pub nvEncGetEncodePresetConfigEx: Option<
        unsafe extern "system" fn(Enc, GUID, GUID, u32, *mut NV_ENC_PRESET_CONFIG) -> NVENCSTATUS,
    >,
    pub nvEncGetSequenceParamEx: Unused,
    pub nvEncRestoreEncoderState: Unused,
    pub nvEncLookaheadPicture: Unused,
    pub reserved2: [*mut c_void; 275],
}

pub type PNvEncodeAPICreateInstance =
    unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
pub type PNvEncodeAPIGetMaxSupportedVersion = unsafe extern "system" fn(*mut u32) -> NVENCSTATUS;

/// All-zero value of a plain-old-data FFI struct (valid for every struct in this module).
pub fn zeroed<T: Copy>() -> T {
    // SAFETY: only used with the POD structs above (integers, raw pointers, Option<fn>).
    unsafe { std::mem::zeroed() }
}

pub fn status_name(s: NVENCSTATUS) -> &'static str {
    const NAMES: [&str; 27] = [
        "NV_ENC_SUCCESS",
        "NV_ENC_ERR_NO_ENCODE_DEVICE",
        "NV_ENC_ERR_UNSUPPORTED_DEVICE",
        "NV_ENC_ERR_INVALID_ENCODERDEVICE",
        "NV_ENC_ERR_INVALID_DEVICE",
        "NV_ENC_ERR_DEVICE_NOT_EXIST",
        "NV_ENC_ERR_INVALID_PTR",
        "NV_ENC_ERR_INVALID_EVENT",
        "NV_ENC_ERR_INVALID_PARAM",
        "NV_ENC_ERR_INVALID_CALL",
        "NV_ENC_ERR_OUT_OF_MEMORY",
        "NV_ENC_ERR_ENCODER_NOT_INITIALIZED",
        "NV_ENC_ERR_UNSUPPORTED_PARAM",
        "NV_ENC_ERR_LOCK_BUSY",
        "NV_ENC_ERR_NOT_ENOUGH_BUFFER",
        "NV_ENC_ERR_INVALID_VERSION",
        "NV_ENC_ERR_MAP_FAILED",
        "NV_ENC_ERR_NEED_MORE_INPUT",
        "NV_ENC_ERR_ENCODER_BUSY",
        "NV_ENC_ERR_EVENT_NOT_REGISTERD",
        "NV_ENC_ERR_GENERIC",
        "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
        "NV_ENC_ERR_UNIMPLEMENTED",
        "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
        "NV_ENC_ERR_RESOURCE_NOT_REGISTERED",
        "NV_ENC_ERR_RESOURCE_NOT_MAPPED",
        "NV_ENC_ERR_NEED_MORE_OUTPUT",
    ];
    NAMES.get(s as usize).copied().unwrap_or("NV_ENC_ERR_<unknown>")
}

// Layout pins (x64 MSVC values from nvEncodeAPI.h n13.0.19.1).
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<GUID>() == 16);
    assert!(size_of::<NV_ENC_CAPS_PARAM>() == 256);
    assert!(size_of::<NV_ENC_RC_PARAMS>() == 128);
    assert!(offset_of!(NV_ENC_RC_PARAMS, averageBitRate) == 20);
    assert!(offset_of!(NV_ENC_RC_PARAMS, vbvBufferSize) == 28);
    assert!(offset_of!(NV_ENC_RC_PARAMS, minQP) == 40);
    assert!(offset_of!(NV_ENC_RC_PARAMS, targetQuality) == 88);
    assert!(offset_of!(NV_ENC_RC_PARAMS, lookaheadDepth) == 90);
    assert!(offset_of!(NV_ENC_RC_PARAMS, qpMapMode) == 96);
    assert!(offset_of!(NV_ENC_RC_PARAMS, multiPass) == 100);
    assert!(offset_of!(NV_ENC_RC_PARAMS, lookaheadLevel) == 112);
    assert!(offset_of!(NV_ENC_RC_PARAMS, reserved1) == 124);
    assert!(size_of::<NV_ENC_CONFIG_H264_VUI_PARAMETERS>() == 112);
    assert!(size_of::<NV_ENC_CONFIG_H264>() == 1792 && align_of::<NV_ENC_CONFIG_H264>() == 8);
    assert!(offset_of!(NV_ENC_CONFIG_H264, level) == 4);
    assert!(offset_of!(NV_ENC_CONFIG_H264, idrPeriod) == 8);
    assert!(offset_of!(NV_ENC_CONFIG_H264, entropyCodingMode) == 44);
    assert!(offset_of!(NV_ENC_CONFIG_H264, maxNumRefFrames) == 60);
    assert!(offset_of!(NV_ENC_CONFIG_H264, h264VUIParameters) == 72);
    assert!(offset_of!(NV_ENC_CONFIG_H264, ltrNumFrames) == 184);
    assert!(offset_of!(NV_ENC_CONFIG_H264, tfLevel) == 220);
    assert!(offset_of!(NV_ENC_CONFIG_H264, reserved1) == 224);
    assert!(offset_of!(NV_ENC_CONFIG_H264, reserved2) == 1280);
    assert!(size_of::<NV_ENC_CODEC_CONFIG>() == 1792);
    assert!(size_of::<NV_ENC_CONFIG>() == 3584);
    assert!(offset_of!(NV_ENC_CONFIG, profileGUID) == 4);
    assert!(offset_of!(NV_ENC_CONFIG, gopLength) == 20);
    assert!(offset_of!(NV_ENC_CONFIG, frameIntervalP) == 24);
    assert!(offset_of!(NV_ENC_CONFIG, rcParams) == 40);
    assert!(offset_of!(NV_ENC_CONFIG, encodeCodecConfig) == 168);
    assert!(offset_of!(NV_ENC_CONFIG, reserved) == 1960);
    assert!(offset_of!(NV_ENC_CONFIG, reserved2) == 3072);
    assert!(size_of::<NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE>() == 16);
    assert!(size_of::<NV_ENC_INITIALIZE_PARAMS>() == 1800);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeWidth) == 36);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, enableEncodeAsync) == 60);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, privDataSize) == 72);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, privData) == 80);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeConfig) == 88);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, maxEncodeWidth) == 96);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, tuningInfo) == 136);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, bufferFormat) == 140);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, outputStatsLevel) == 148);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, reserved1) == 152);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, reserved2) == 1288);
    assert!(size_of::<NV_ENC_RECONFIGURE_PARAMS>() == 1816);
    assert!(offset_of!(NV_ENC_RECONFIGURE_PARAMS, reInitEncodeParams) == 8);
    assert!(offset_of!(NV_ENC_RECONFIGURE_PARAMS, flags) == 1808);
    assert!(offset_of!(NV_ENC_RECONFIGURE_PARAMS, reserved2) == 1812);
    assert!(size_of::<NV_ENC_PRESET_CONFIG>() == 5128);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, presetCfg) == 8);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, reserved1) == 3592);
    assert!(size_of::<NV_ENC_PIC_PARAMS>() == 3360);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, inputTimeStamp) == 24);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, inputBuffer) == 40);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, completionEvent) == 56);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, bufferFmt) == 64);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, pictureType) == 72);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, codecPicParams) == 80);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, meHintCountsPerBlock) == 1624);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, meExternalHints) == 1656);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, qpDeltaMap) == 1712);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, meHintRefPicDist) == 1728);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, alphaBuffer) == 1736);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, stateBufferIdx) == 1756);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, outputReconBuffer) == 1760);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, reserved3) == 1768);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, reserved6) == 2904);
    assert!(size_of::<NV_ENC_LOCK_BITSTREAM>() == 1544);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputBitstream) == 8);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes) == 36);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputTimeStamp) == 40);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr) == 56);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, pictureType) == 64);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputStatsPtr) == 120);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, frameIdxDisplay) == 128);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, reserved1) == 132);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, reserved2) == 1008);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, reservedInternal) == 1512);
    assert!(size_of::<NV_ENC_MAP_INPUT_RESOURCE>() == 1544);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, registeredResource) == 16);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mappedResource) == 24);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mappedBufferFmt) == 32);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, reserved2) == 1040);
    assert!(size_of::<NV_ENC_REGISTER_RESOURCE>() == 1536);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, resourceToRegister) == 24);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, registeredResource) == 32);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, bufferFormat) == 40);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, pInputFencePoint) == 48);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, chromaOffset) == 56);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, reserved2) == 1048);
    assert!(size_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>() == 776);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer) == 16);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, reserved2) == 264);
    assert!(size_of::<NV_ENC_EVENT_PARAMS>() == 1544);
    assert!(offset_of!(NV_ENC_EVENT_PARAMS, completionEvent) == 8);
    assert!(size_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>() == 1552);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device) == 8);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion) == 24);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved2) == 1040);
    assert!(size_of::<NV_ENCODE_API_FUNCTION_LIST>() == 2552);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx) == 240);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncReconfigureEncoder) == 264);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx) == 320);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncLookaheadPicture) == 344);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, reserved2) == 352);
    // Version words as computed by the C macros.
    assert!(NV_ENC_CONFIG_VER == 0xF009_000D);
    assert!(NV_ENC_INITIALIZE_PARAMS_VER == 0xF007_000D);
    assert!(NV_ENC_RECONFIGURE_PARAMS_VER == 0xF002_000D);
    assert!(NV_ENC_PRESET_CONFIG_VER == 0xF005_000D);
    assert!(NV_ENC_PIC_PARAMS_VER == 0xF007_000D);
    assert!(NV_ENC_LOCK_BITSTREAM_VER == 0xF002_000D);
    assert!(NV_ENC_MAP_INPUT_RESOURCE_VER == 0x7004_000D);
    assert!(NV_ENC_REGISTER_RESOURCE_VER == 0x7005_000D);
    assert!(NV_ENC_CREATE_BITSTREAM_BUFFER_VER == 0x7001_000D);
    assert!(NV_ENC_EVENT_PARAMS_VER == 0x7002_000D);
    assert!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER == 0x7001_000D);
    assert!(NV_ENCODE_API_FUNCTION_LIST_VER == 0x7002_000D);
    assert!(NV_ENC_RC_PARAMS_VER == 0x7001_000D);
};
