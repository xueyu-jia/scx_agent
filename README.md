# 赛题题目：面向openEuler的自适应资源管控Agent（社区赛题）

## 赛题说明：
设计并实现一个基于用户态调度的资源管控Agent框架，例如用户态调度的agent，通过agent感知workload然后基于sched_ext调整cpu调度策略，生成scx，优化workload性能；除了上面的cpu调度agent，还可以考虑将ebpf作为hook，agent作为策略决策，实现其他类型的os agent；比如：network policy agent，security policy agent, resource contral agent(cgroup)等。
## 赛题要求：
- 设计并实现一个基于用户态调度的资源管控Agent框架，实现标准化的工具和Skills能力接口，支持Agent快速构建和扩展；
- Agent需要能够感知workloads，基于sched_ext调整CPU调度策略；
- 集成scx调度器，实现对workload性能的优化；
- 支持扩展ebpf作为hook，实现network policy agent、security policy agent、resource control agent等功能；
- 提供完整的性能对比测试数据和可复现的实验环境。
## 交付要求：
   最终交付的代码需在 openEuler 24.03-LTS-SP3 操作系统版本上能够正常编译、运行和测试，鼓励在更多Linux发行版上编译、运行和测试。
## 评分细则：
- 创新性（30分）：Agent架构设计的新颖性，调度策略的创新程度；
- 功能完整性（25分）：对workload的感知能力、调度策略调整的准确性和灵活性；
- 性能提升（25分）：相比默认调度器的性能提升幅度；
- 代码质量（10分）：代码结构、注释和可维护性；
- 演示效果（10分）：现场演示的流畅度和说服力。
## 赛题联系人：
段老师  duanpengjie@huawei.com 
## 参考资料：
https://mp.weixin.qq.com/s/ewC8gTMRvm2NO6ywoproAA