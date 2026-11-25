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
    // execution: the shared minute, named once in the stage log after ten
    // seconds. An OS wait that cannot settle is explicitly unconfirmed.
    const int CleanupBudgetMs = 60000;
    const int WarnAfterMs = 10000;
    static void Warn(string operation, long elapsedMs) {
        if (quiet) return;
        Report("warning: " + operation + " is still running after 10 seconds; attempt="
            + Process.GetCurrentProcess().Id + "/0; elapsed_ms=" + elapsedMs);
    }
    // Warnings reach stderr from their own writer thread, never from the
    // thread doing the work, so a slow write delays only the line. Before the
    // helper exits, FlushWarnings gives queued lines what remains of its one
    // second.
    static readonly System.Collections.Concurrent.BlockingCollection<string> warningLines =
        new System.Collections.Concurrent.BlockingCollection<string>();
    static readonly object writerGate = new object();
    static Thread warningWriter;
    public static void Report(string line) {
        try {
            lock (writerGate) {
                if (warningWriter == null) {
                    // Its own stderr stream, not the shared Console.Error
                    // writer, so a slow write never holds the lock the
                    // helper's own error output needs.
                    warningWriter = new Thread(() => {
                        System.IO.Stream stderr = null;
                        try { stderr = Console.OpenStandardError(); } catch { /* lines are then not written */ }
                        foreach (string text in warningLines.GetConsumingEnumerable()) {
                            try {
                                byte[] bytes = Encoding.UTF8.GetBytes(text + Environment.NewLine);
                                stderr.Write(bytes, 0, bytes.Length);
                                stderr.Flush();
                            } catch { /* observation never fails the work */ }
                        }
                    });
                    warningWriter.IsBackground = true;
                    warningWriter.Start();
                }
            }
            warningLines.Add(line);
        } catch { /* observation never fails the work */ }
    }
    public static void FlushWarnings(int allowanceMs) {
        try {
            warningLines.CompleteAdding();
            Thread writer;
            lock (writerGate) { writer = warningWriter; }
            if (writer != null) writer.Join(Math.Max(0, allowanceMs));
        } catch { /* observation never fails the work */ }
    }
    static bool ConfirmCleanup(IntPtr job, Stopwatch clock, out uint active) {
        active = UInt32.MaxValue;
        bool warned = false;
        do {
            Accounting accounting;
            bool queried = QueryInformationJobObject(job, 1, out accounting,
                (uint)Marshal.SizeOf(typeof(Accounting)), IntPtr.Zero);
            long elapsed = clock.ElapsedMilliseconds;
            if (!warned && elapsed >= WarnAfterMs) {
                warned = true;
                Warn("process job cleanup confirmation", elapsed);
            }
            if (!queried) return false;
            active = accounting.ActiveProcesses;
            if (active == 0) return true;
            if (elapsed >= CleanupBudgetMs) return false;
            Thread.Sleep(10);
        } while (true);
    }
    static bool WaitWithWarning(IntPtr handle) {
        if (WaitForSingleObject(handle, (uint)WarnAfterMs) == 0) return true;
        Warn("suspended process cleanup confirmation", WarnAfterMs);
        return WaitForSingleObject(handle, (uint)(CleanupBudgetMs - WarnAfterMs)) == 0;
    }
    // Set when the parent already names the operation this helper serves, such
    // as a preflight probe: then this helper names nothing itself.
    static bool quiet;
    // `started` is told once the workload has resumed, which ends the
    // supervisor's startup; the PowerShell bootstrap names a slow startup.
    public static Outcome Run(string command, string[] args, string cwd, string[] environment,
        Action started, bool parentNamesOperation) {
        quiet = parentNamesOperation;
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
            // Startup ends here.
            if (started != null) started();
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
                cleanup = TerminateProcess(process.process, 1) && WaitWithWarning(process.process);
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
