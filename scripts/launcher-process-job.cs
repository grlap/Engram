using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

// This supervisor stays outside the job. The workload cannot run, or spawn,
// before assignment. Descendants cannot break away from the owned job.
public static class LauncherProcessJob
{
    public sealed class Outcome {
        public int ExitCode { get; set; }
        public bool Stopped { get; set; }
        public bool CleanupConfirmed { get; set; }
        public uint ActiveProcesses { get; set; }
        public string Phase { get; set; }
        public string Error { get; set; }
    }
    [StructLayout(LayoutKind.Sequential)] struct IoCounters {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
        public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }
    [StructLayout(LayoutKind.Sequential)] struct BasicLimits {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass, SchedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)] struct ExtendedLimits {
        public BasicLimits BasicLimitInformation;
        public IoCounters IoInfo;
        public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }
    [StructLayout(LayoutKind.Sequential)] struct Accounting {
        public long TotalUserTime, TotalKernelTime, ThisPeriodTotalUserTime, ThisPeriodTotalKernelTime;
        public uint TotalPageFaultCount, TotalProcesses, ActiveProcesses, TotalTerminatedProcesses;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct StartupInfo {
        public uint cb;
        public string reserved, desktop, title;
        public uint x, y, xSize, ySize, xCountChars, yCountChars, fillAttribute, flags;
        public ushort showWindow, reserved2;
        public IntPtr reservedPointer, stdin, stdout, stderr;
    }
    [StructLayout(LayoutKind.Sequential)] struct ProcessInfo {
        public IntPtr process, thread;
        public uint processId, threadId;
    }
    [StructLayout(LayoutKind.Sequential)] struct SecurityAttributes {
        public uint length;
        public IntPtr descriptor;
        [MarshalAs(UnmanagedType.Bool)] public bool inherit;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFile(string name, uint access, uint share, ref SecurityAttributes security,
        uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr job, int kind, ref ExtendedLimits info, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool QueryInformationJobObject(IntPtr job, int kind, out Accounting info, uint size, IntPtr returned);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateJobObject(IntPtr job, uint code);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcess(string application, StringBuilder command, IntPtr processAttributes,
        IntPtr threadAttributes, bool inherit, uint flags, IntPtr environment, string cwd,
        ref StartupInfo startup, out ProcessInfo process);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr process, out uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int kind);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DuplicateHandle(IntPtr from, IntPtr source, IntPtr to, out IntPtr target,
        uint access, bool inherit, uint options);
    static void Check(bool ok) { if (!ok) throw new Win32Exception(Marshal.GetLastWin32Error()); }
    static IntPtr InheritedOutput(int kind) {
        IntPtr handle;
        Check(DuplicateHandle(GetCurrentProcess(), GetStdHandle(kind), GetCurrentProcess(), out handle, 0, true, 2));
        return handle;
    }
    // CommandLineToArgvW/CRT quoting; no shell interprets the command.
    static string Quote(string value) {
        var text = new StringBuilder("\"");
        int slashes = 0;
        foreach (char c in value) {
            if (c == '\\') { slashes++; continue; }
            if (c == '"') { text.Append('\\', slashes * 2 + 1); text.Append(c); }
            else { text.Append('\\', slashes); text.Append(c); }
            slashes = 0;
        }
        text.Append('\\', slashes * 2); text.Append('"'); return text.ToString();
    }
    // This bounds confirmation after cancellation or root exit, not stage
    // execution. An OS wait that cannot settle is explicitly unconfirmed.
    const int CleanupBudgetMs = 30000;
    static bool ConfirmCleanup(IntPtr job, Stopwatch clock, out uint active) {
        active = UInt32.MaxValue;
        do {
            Accounting accounting;
            if (!QueryInformationJobObject(job, 1, out accounting,
                (uint)Marshal.SizeOf(typeof(Accounting)), IntPtr.Zero)) return false;
            active = accounting.ActiveProcesses;
            if (active == 0) return true;
            if (clock.ElapsedMilliseconds >= CleanupBudgetMs) return false;
            Thread.Sleep(10);
        } while (true);
    }
    public static Outcome Run(string command, string[] args, string cwd, string[] environment) {
        IntPtr job = IntPtr.Zero, input = IntPtr.Zero, output = IntPtr.Zero, error = IntPtr.Zero;
        IntPtr environmentBlock = IntPtr.Zero;
        ProcessInfo process = new ProcessInfo();
        bool assigned = false, cleanup = false, stopped = false;
        try {
            job = CreateJobObject(IntPtr.Zero, null);
            Check(job != IntPtr.Zero);
            var limits = new ExtendedLimits();
            limits.BasicLimitInformation.LimitFlags = 0x2000; // KILL_ON_JOB_CLOSE, no BREAKAWAY_OK
            Check(SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf(typeof(ExtendedLimits))));
            output = InheritedOutput(-11); error = InheritedOutput(-12);
            var security = new SecurityAttributes { length = (uint)Marshal.SizeOf(typeof(SecurityAttributes)), inherit = true };
            input = CreateFile("NUL", 0x80000000, 3, ref security, 3, 0, IntPtr.Zero);
            Check(input != new IntPtr(-1));
            var startup = new StartupInfo();
            startup.cb = (uint)Marshal.SizeOf(typeof(StartupInfo));
            startup.flags = 0x100; startup.stdin = input;
            startup.stdout = output; startup.stderr = error;
            var line = new StringBuilder(Quote(command));
            foreach (string arg in args) line.Append(" ").Append(Quote(arg));
            Array.Sort(environment, StringComparer.OrdinalIgnoreCase);
            environmentBlock = Marshal.StringToHGlobalUni(String.Join("\0", environment) + "\0\0");
            Check(CreateProcess(command, line, IntPtr.Zero, IntPtr.Zero, true,
                0x08000404, environmentBlock, cwd, ref startup, out process)); // NO_WINDOW | UNICODE_ENVIRONMENT | SUSPENDED
            Check(AssignProcessToJobObject(job, process.process));
            assigned = true;
            Check(ResumeThread(process.thread) != 0xffffffff);
            // A parent EOF also tears down the tree. Only the parent writes here.
            Task<string> cancellation = Task.Run(() => Console.In.ReadLine());
            while (true) {
                uint wait = WaitForSingleObject(process.process, 25);
                Check(wait != 0xffffffff);
                if (cancellation.IsCompleted) {
                    stopped = true;
                    break;
                }
                if (wait == 0) break;
            }
            uint exit = 1;
            if (!stopped) Check(GetExitCodeProcess(process.process, out exit));
            // Root exit does not prove descendant exit. Retain the job and
            // remove any residual members on normal completion as well.
            var clock = Stopwatch.StartNew();
            Check(TerminateJobObject(job, 1));
            uint active;
            cleanup = ConfirmCleanup(job, clock, out active);
            return new Outcome { ExitCode = stopped ? 1 : unchecked((int)exit), Stopped = stopped,
                CleanupConfirmed = cleanup, ActiveProcesses = active, Phase = "workload" };
        } catch (Exception failure) {
            uint active = 0;
            if (process.process == IntPtr.Zero) cleanup = true;
            else if (!assigned) {
                // Assignment failure leaves only a suspended process. No
                // workload instruction has run, but cleanup still needs proof.
                cleanup = TerminateProcess(process.process, 1)
                    && WaitForSingleObject(process.process, CleanupBudgetMs) == 0;
                if (!cleanup) active = 1;
            } else {
                var clock = Stopwatch.StartNew();
                cleanup = TerminateJobObject(job, 1) && ConfirmCleanup(job, clock, out active);
                if (!cleanup && active == 0) active = UInt32.MaxValue;
            }
            return new Outcome { ExitCode = 1, Stopped = stopped, CleanupConfirmed = cleanup,
                ActiveProcesses = active, Phase = assigned ? "workload" : "launch", Error = failure.Message };
        } finally {
            // Assignment failure leaves a suspended process: it must never run.
            if (process.process != IntPtr.Zero && !assigned) {
                TerminateProcess(process.process, 1);
            }
            if (job != IntPtr.Zero) {
                if (!cleanup) TerminateJobObject(job, 1);
                CloseHandle(job);
            }
            if (process.thread != IntPtr.Zero) CloseHandle(process.thread);
            if (process.process != IntPtr.Zero) CloseHandle(process.process);
            if (input != IntPtr.Zero && input != new IntPtr(-1)) CloseHandle(input);
            if (output != IntPtr.Zero) CloseHandle(output);
            if (error != IntPtr.Zero) CloseHandle(error);
            if (environmentBlock != IntPtr.Zero) Marshal.FreeHGlobal(environmentBlock);
        }
    }
}
