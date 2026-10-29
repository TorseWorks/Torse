# CPU执行器

## 借用批次

除固定池`Executor<T>`外，`run_scoped`提供会在返回前回收全部线程的借用批次。调用前检查总任务数与每项输入、输出预留之和；工作共享只读上下文，结果按输入序号返回。回调panic作为该项错误返回，其余工作仍完成。线程启动失败时也等待已启动工作结束。

该入口用于权威阶段起点的只读规划，避免复制完整世界；每批创建有限线程，不复用固定池，也不提供跨批次异步结果或取消。调用方在整批返回后才能继续修改被借用的数据。原生适配器负责约束中间分配，批次上限包含完成但尚未归并的结果。

## 固定池

`Executor<T>`使用固定工作线程处理可信的原生CPU任务。先预留任务槽、输入字节和工作及输出字节，再构造拥有所有权的输入，最后提交计算。工作线程不持有世界写权限。

```rust
use torse_runtime::{Executor, ExecutorConfig, JobBudget, JobOutput, Priority};

let executor = Executor::new(ExecutorConfig {
    workers: 2,
    max_jobs: 8,
    max_input_bytes: 1024,
    max_output_bytes: 1024,
}).expect("valid limits");
let reservation = executor.try_reserve(JobBudget {
    input_bytes: 4,
    output_bytes: 4,
}).expect("capacity available");
let input = 17u32;
let mut job = reservation.submit(Priority::Visible, move |cancel| {
    cancel.check()?;
    Ok(JobOutput { value: input * 2, bytes: 4 })
});
if let Some(result) = job.try_take() {
    let value = result.expect("CPU calculation succeeded");
    assert_eq!(value, 34);
}
```

| 边界 | 行为 |
|---|---|
| 容量 | 预留、排队、运行和完成未取走的任务共同计费；结果队列使用相同任务槽 |
| Busy | 本次不接纳，由调用方保留需求并在之后重试；不在工作线程等待空位 |
| TooLarge | 单任务预算超过总上限，应调整任务粒度或报告错误 |
| 取消 | 丢弃Job取消排队工作并释放输入；运行工作保持计费，直到回调结束；晚到结果不会交付 |
| 保留句柄 | 调用cancel后继续持有Job，仍保留容量，直到句柄及工作引用释放 |
| 字节预算 | 可信适配器声明输入、峰值临时缓冲和输出容量；返回结果复核输出字节，不作为任意代码的内存沙箱 |
| 优先级 | Visible优先；连续领取最多8项后服务等待的Background任务，同优先级保持FIFO |
| 故障 | 当前展开式panic配置下，派生计算的panic返回Panicked，该线程继续接收任务 |
| 关闭 | 丢弃Executor停止接纳、取消排队和运行工作并回收线程；回调必须定期检查取消 |

锁只保护短期队列和结果操作，计算在锁外执行。输入与输出缓冲由调用方按容量计费，线程栈、任务控制对象和分配器开销不等于上述字节量；控制对象数量受max_jobs和workers约束。结果取走后由调用方另行管理缓存预算。

该接口不暴露阻塞等待结果的方法。工作线程不执行磁盘、网络操作，也不等待同池子任务；生成与网格适配器按高度行或切片检查取消。执行器不强制终止任意原生代码。

生成与网格接入有界执行器的取消、背压与结果失效验证见[记录](../../benchmarks/archive/performance/M6-bounded-executor.md)。ECS实验仍通过ecs-probe特性启用。

`Executor::with_memory`可与世界、复制和资源共用MemoryPool，任务槽和输入、输出峰值均参与同一总量校验。`Job::try_take_charged`将完成结果的原预留转入指定分类，分类暂满时保留结果供重试；`try_take`仍由调用方自行负责结果的常驻预算。两端主程序装配及范围见[共享内存记录](../../benchmarks/archive/performance/M6-shared-memory.md)。
