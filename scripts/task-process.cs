// Windows task containment. The job handle belongs only to the calling script.
using System;
using System.Collections.Concurrent;
using System.ComponentModel;
using System.IO;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Freym {
    public sealed class TaskProcess : IDisposable {
        [StructLayout(LayoutKind.Sequential)] struct Limits {
            public long ProcessTime, JobTime;
            public uint Flags;
            public UIntPtr MinWorkingSet, MaxWorkingSet;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint Priority, Scheduling;
            public ulong ReadOps, WriteOps, OtherOps, ReadBytes, WriteBytes, OtherBytes;
            public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
        }
        [StructLayout(LayoutKind.Sequential)] struct Accounting {
            public long UserTime, KernelTime, PeriodUserTime, PeriodKernelTime;
            public uint PageFaults, TotalProcesses, ActiveProcesses, TerminatedProcesses;
        }
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct Startup {
            public int Size;
            public string Reserved, Desktop, Title;
            public uint X, Y, Width, Height, XChars, YChars, Fill, Flags;
            public ushort Show, ReservedSize;
            public IntPtr ReservedData, Input, Output, Error;
        }
        [StructLayout(LayoutKind.Sequential)] struct ProcessInfo {
            public IntPtr Process, Thread;
            public uint ProcessId, ThreadId;
        }
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern IntPtr CreateJobObjectW(IntPtr security, string name);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool SetInformationJobObject(IntPtr job, int kind, ref Limits limits, uint size);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool QueryInformationJobObject(IntPtr job, int kind, out Accounting accounting, uint size, IntPtr returned);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool CreateProcessW(string file, StringBuilder command, IntPtr processSecurity,
            IntPtr threadSecurity, bool inherit, uint flags, IntPtr environment, string directory,
            ref Startup startup, out ProcessInfo info);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
        [DllImport("kernel32.dll", SetLastError = true)] static extern uint ResumeThread(IntPtr thread);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateJobObject(IntPtr job, uint code);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr process, uint code);
        [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
        [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr process, out uint code);
        [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int kind);
        [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);

        IntPtr job, process;
        AnonymousPipeServerStream pipe, errorPipe;
        Task reader;
        public sealed class OutputLine {
            public string Text;
            public bool IsError;
        }
        readonly ConcurrentQueue<OutputLine> output = new ConcurrentQueue<OutputLine>();
        public uint Id { get; private set; }

        static void Check(bool ok) { if (!ok) throw new Win32Exception(Marshal.GetLastWin32Error()); }

        public TaskProcess(string file, string command, string directory) {
            ProcessInfo info = default;
            try {
                // Unnamed and non-inheritable: only this owner can close the job.
                job = CreateJobObjectW(IntPtr.Zero, null);
                Check(job != IntPtr.Zero);
                var limits = new Limits { Flags = 0x2000 }; // KILL_ON_JOB_CLOSE; no breakaway.
                Check(SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf<Limits>()));
                pipe = new AnonymousPipeServerStream(PipeDirection.In, HandleInheritability.Inheritable);
                errorPipe = new AnonymousPipeServerStream(PipeDirection.In, HandleInheritability.Inheritable);
                var startup = new Startup {
                    Size = Marshal.SizeOf<Startup>(), Flags = 0x100, // USESTDHANDLES
                    Input = GetStdHandle(-10),
                    Output = pipe.ClientSafePipeHandle.DangerousGetHandle(),
                    Error = errorPipe.ClientSafePipeHandle.DangerousGetHandle()
                };
                // Assign before executing any child code, so even fast grandchildren are owned.
                Check(CreateProcessW(file, new StringBuilder(command), IntPtr.Zero, IntPtr.Zero,
                    true, 0x08000004, IntPtr.Zero, directory, ref startup, out info)); // NO_WINDOW | SUSPENDED
                process = info.Process;
                Id = info.ProcessId;
                Check(AssignProcessToJobObject(job, process));
                Check(ResumeThread(info.Thread) != uint.MaxValue);
                pipe.DisposeLocalCopyOfClientHandle();
                errorPipe.DisposeLocalCopyOfClientHandle();
                reader = Task.WhenAll(ReadOutput(pipe, false), ReadOutput(errorPipe, true));
            } catch {
                // This handle is the suspended process we just created, never a PID lookup.
                if (process != IntPtr.Zero) { TerminateProcess(process, 1); WaitForSingleObject(process, 5000); }
                if (job != IntPtr.Zero) CloseHandle(job);
                if (process != IntPtr.Zero) CloseHandle(process);
                pipe?.Dispose();
                errorPipe?.Dispose();
                throw;
            } finally {
                if (info.Thread != IntPtr.Zero) CloseHandle(info.Thread);
            }
        }

        Task ReadOutput(Stream source, bool isError) {
            return Task.Run(() => {
                using (var stream = new StreamReader(source, new UTF8Encoding(false))) {
                    string line;
                    while ((line = stream.ReadLine()) != null)
                        output.Enqueue(new OutputLine { Text = line, IsError = isError });
                }
            });
        }

        public bool Wait(uint milliseconds) {
            uint result = WaitForSingleObject(process, milliseconds);
            Check(result != uint.MaxValue);
            return result == 0;
        }
        public int ExitCode {
            get { uint code; Check(GetExitCodeProcess(process, out code)); return unchecked((int)code); }
        }
        public OutputLine[] DrainOutput() {
            var lines = new System.Collections.Generic.List<OutputLine>();
            OutputLine line;
            while (output.TryDequeue(out line)) lines.Add(line);
            return lines.ToArray();
        }
        public void Dispose() {
            if (job == IntPtr.Zero) return;
            try {
                Check(TerminateJobObject(job, 1));
                // Wait for every descendant, including children whose parent already exited.
                var deadline = System.Diagnostics.Stopwatch.StartNew();
                Accounting accounting;
                do {
                    Check(QueryInformationJobObject(job, 1, out accounting, (uint)Marshal.SizeOf<Accounting>(), IntPtr.Zero));
                    if (accounting.ActiveProcesses == 0) break;
                    if (deadline.ElapsedMilliseconds > 5000) throw new TimeoutException("Task process cleanup did not finish.");
                    Thread.Sleep(10);
                } while (true);
                reader?.GetAwaiter().GetResult();
            } finally {
                // Also works when the supervising PowerShell process is forcibly interrupted:
                // Windows closes its sole job handle and kills only this job's members.
                CloseHandle(job); job = IntPtr.Zero;
                if (process != IntPtr.Zero) { CloseHandle(process); process = IntPtr.Zero; }
                pipe?.Dispose();
                errorPipe?.Dispose();
            }
        }
    }
}
