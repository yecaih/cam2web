# V4L2 UAPI bindings

`bindings.rs` 由 bindgen 0.72.1 从 Ubuntu `linux-libc-dev` 7.0.0-43.43 的 `linux/videodev2.h` 生成。用 x86_64 和 AArch64 的同版本 UAPI 头文件生成的结果一致。请求号与内核 ABI 的 `timeval` 类型由 `linux-raw-sys` 按目标架构提供；`ffi.rs` 在编译期校验每个请求编码中的参数大小。

一般构建直接使用 `bindings.rs`，不用安装 bindgen 或 V4L2 头文件。只有更新绑定时才在仓库根目录运行下面的命令（以 x86_64 Linux 上的头文件路径为例）：

```sh
bindgen /usr/include/linux/videodev2.h \
  --allowlist-type 'v4l2_(capability|fmtdesc|frmsizeenum|frmivalenum|query_ext_ctrl|querymenu|ext_controls|format|streamparm|requestbuffers|buffer|field|buf_type|memory|frmsizetypes|frmivaltypes|ctrl_type)' \
  --allowlist-var 'V4L2_(CAP_(DEVICE_CAPS|VIDEO_CAPTURE|VIDEO_CAPTURE_MPLANE|META_CAPTURE|STREAMING|TIMEPERFRAME)|BUF_FLAG_ERROR|CTRL_(WHICH_CUR_VAL|FLAG_NEXT_CTRL|FLAG_NEXT_COMPOUND))' \
  --blocklist-type timeval \
  --raw-line 'use linux_raw_sys::general::__kernel_old_timeval as timeval;' \
  --opaque-type 'v4l2_ctrl_.*' \
  --no-layout-tests --rust-target 1.88 --no-doc-comments --no-derive-debug \
  --with-derive-default --no-prepend-enum-name \
  -o src/v4l/uapi/bindings.rs
```

更新后至少检查本机测试、AArch64 构建，以及目标板上的 RGB / ToF MMAP 采集与控件读写。
