# 24 层驻留列卸载对照

本机 Windows 11 26200、Ryzen 7 7840H、NTFS、Rust 1.93.1 Release，使用真实页文件和文件日志。每轮从独立世界开始，安装和生成均不计时。串行路径逐页调用 `unload_section`。批量路径设置 24 页上限，从首次 `poll_residency` 测到全部 24 页耐久确认。每种路径记录三轮。

复现命令

```sh
cargo test --release --locked -p torse-server --test parking compare_serial_and_batched_retirement -- --ignored --nocapture --test-threads=1
```

- 串行路径最长单次宿主调用为 2.7749、3.2368、3.1943 ms。整列耐久确认用时 54.8337、56.7808、56.5686 ms。每轮页文件数据同步 24 次、日志同步请求 24 次。
- 批量路径最长单次宿主调用为 1.5233、1.2819、1.6782 ms。整列耐久确认用时 19.8271、16.2185、19.2761 ms。每轮页文件数据同步 1 次，当前日志预算将发布拆为六个耐久前缀。

`retirement_batches_sync_once_and_recheck_foldback` 另外检查读写任务同时在途、折返页不被卸载、前方已停放页可恢复。上述计时未执行集成跑图、GPU 呈现或硬件断电验证。
