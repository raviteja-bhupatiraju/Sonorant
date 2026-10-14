//! Pinned model files: the commit each was taken from, its size and SHA-256. Generated from the
//! Hugging Face API at the commit; update a row (and test the model) to move to a newer revision.
//! A downloaded file that does not match is rejected, so a changed upstream repository cannot
//! feed different bytes to the model loaders.

/// `(repo, file, commit, size in bytes, sha256)`
pub const PINS: &[(&str, &str, &str, u64, &str)] = &[
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "onnx/model.onnx", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 325532232, "8fbea51ea711f2af382e88c833d9e288c6dc82ce5e98421ea61c058ce21a34cb"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "onnx/model_quantized.onnx", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 92361116, "fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_alloy.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "c4a6b876047fd7fb472edf4ebd63cfac7c3b958a7cae7c106e8f038ca6308c45"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_aoede.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "4a004c33430762e2461eedb2013fad808ef4ab3121f5300f554476caf58d8361"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_bella.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "f69d836209b78eb8c66e75e3cda491e26ea838a3674257e9d4e5703cbaf55c8b"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_heart.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_jessica.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "a240a5e3c15b43563d6e923bdca8ef5613a23471d9b77653694012435df23bd8"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_kore.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "9be5221b6a941c04b561959b8ff0b06e809444dcc4ab7e75a7b23606f691819e"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_nicole.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "cd2191ab31b914ed7b318416b0e4440fdf392ddad9106a060819aa600a64f59a"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_nova.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "18778272caa0d0eebaea251c35fd635f038434f9eee5e691d02a174bd328414f"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_river.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "00a2bcf82b1d86e8f19902ede58c65ccf6c0e43b44b7d74fad54e5d8933c9c30"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_sarah.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "4409fbc125afabacc615d94db5398d847006a737b0247d6892b7a9a0007a2f0a"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_sky.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "4435255c9744f3f31659e0d714ab7689bf65d9e77ec1cce060f083912614f0b9"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_adam.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "162b035ed91cfc48b6046982184c645f72edcdd1b82843347f605d7bf7b15716"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_echo.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "3968b92c3c4cd1c4416dbded36c13eaa388a90d5788d02a13e4d781f5f8cf3c3"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_eric.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "e8b5be17edd1e3636901ce7598baafe2dc8dd8ff707a0c23bf9e461add7e2832"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_fenrir.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "c27989f741f7ee34d273a39d8a595cc0837d35f5ced9a29b7cc162614616df43"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_liam.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "52403be32fd047c6a44517cb0bcd6b134f2a18baa73e70ef41651e0eab921ade"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_michael.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "1d1f21dd8da39c30705cd4c75d039d265e9bc4a2a93ed09bc9e1b1225eb95ba1"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_onyx.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "da5d135b424164916d75a68ffb4c2abce3d7d5ccc82dd1ee6cf447ce286145e6"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_puck.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "fcf73c989033e9233e0b98713eca600c8c74dcc1614b37009d5450ff4a2274a0"),
    ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/am_santa.bin", "1939ad2a8e416c0acfeecc08a694d14ef25f2231", 522240, "61150cf726ab6c5ed7a99f90a304f91f5a72c00c592e89ec94e5df11c319227a"),
    ("handy-computer/parakeet-unified-en-0.6b-gguf", "parakeet-unified-en-0.6b-Q8_0.gguf", "d5249700b2382bf5c5024c2421d101b8db54a629", 731357568, "4b50b6dd862bf6e346929aaf4f5eaacec003bfa3f56462d6c874b41ef2f38795"),
    ("handy-computer/canary-180m-flash-gguf", "canary-180m-flash-Q8_0.gguf", "46b86384d1db59108d6e060a0fc00924cb58499f", 218447648, "f942ab746040b531a29ab78ca3afc8dc9564efd8e4c942b5541e53644be9c408"),
    ("Cactus-Compute/whistle", "whistle.cact", "ca5287601bef25af26dcf2e1b2bdc0843a7c19e5", 16919407, "b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb"),
    ("onnx-community/silero-vad", "onnx/model.onnx", "e71cae966052b992a7eca6b17738916ce0eca4ec", 2243022, "a4a068cd6cf1ea8355b84327595838ca748ec29a25bc91fc82e6c299ccdc5808"),
];
