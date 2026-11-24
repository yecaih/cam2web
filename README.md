# cam2web

一个 Linux V4L2 相机调试工具：在浏览器里看实时预览、读写相机控件。

## 运行

```sh
cargo run --release -- config/orin.toml
```

`config/orin.toml` 是 Jetson Orin 上的 Himax RGB / ToF 示例；其他相机请换成对应的配置。已有可执行文件时，直接运行 `./cam2web config/orin.toml`。

用示例配置启动后，打开 <http://localhost:8766/>（远程访问时把 `localhost` 换成板子的 IP），选中相机，点击「启动预览」。运行账号需要有 `/dev/video*` 的访问权限。服务无鉴权，请只在可信网络使用。

## 采集节点与控件节点

选中 `/dev/videoN` 后，cam2web 根据设备号从 `/dev/media*` 的 Media Controller 拓扑沿启用的链路向上找传感器。能唯一定位并打开 `/dev/v4l-subdevN` 时，两边共有的控件优先走传感器节点，周期读值也不再占用采集线程；采集和仅由视频节点暴露的控件仍走 `/dev/videoN`。找不到唯一的传感器节点或没有访问权限时，保留原来的视频节点路径。

不需要配置 subdev 编号。要启用这条路径，运行账号还需要能读取 `/dev/media*`，并读写对应的 `/dev/v4l-subdev*`；如果在程序外切换了媒体链路，请重新选择相机。

## 交叉编译

使用 musl 库，默认静态编译
```sh
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```