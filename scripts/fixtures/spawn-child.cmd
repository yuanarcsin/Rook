@echo off
rem 夹具：cmd 启动器 —— 派生孙进程后**父进程立即退出**，孙进程继续长驻。
rem
rem 用法：spawn-child.cmd <node-exe> <pidfile> [hold-ms=60000]
rem   node-exe  孙进程使用的 Node 绝对路径（由测试传入，避免依赖 PATH）
rem   pidfile   孙进程把自己的 PID 写到这里
rem   hold-ms   孙进程保持时长（毫秒）
rem
rem 输出：
rem   "launcher spawning pidfile=<pidfile>"
rem   孙进程自己的 "grandchild ready pid=<pid>"
rem
rem 退出码：0（启动器立即退出；孙进程被 Job 回收时为 1）
rem
rem 为什么用 cmd 而不是 Node 当启动器：Node（libuv）会为每个非 detached 子进程
rem 建立带 KILL_ON_JOB_CLOSE 的 Job，父进程一退出就连带杀死子进程，无法构造
rem "父进程退出、子进程仍在"的场景。cmd 的 start /b 没有这层 Job，孙进程会
rem 继承 Rook 自己的 Job，因此仍可被 TerminateJobObject 回收。

setlocal
set "NODE=%~1"
set "PIDFILE=%~2"
set "HOLD=%~3"
if "%HOLD%"=="" set "HOLD=60000"

echo launcher spawning pidfile=%PIDFILE%
start "" /b "%NODE%" "%~dp0grandchild.mjs" "%PIDFILE%" %HOLD%
exit /b 0
