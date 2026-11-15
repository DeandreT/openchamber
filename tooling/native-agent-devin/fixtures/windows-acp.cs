using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text.RegularExpressions;

// Credential-free console executable. It starts no tools or child processes.
class WindowsAcp {
  [DllImport("kernel32.dll")] static extern IntPtr GetConsoleWindow();
  static void Reply(string id, string result) {
    Console.WriteLine("{\"jsonrpc\":\"2.0\",\"id\":" + id + ",\"result\":" + result + "}");
  }
  static void Main(string[] args) {
    if (args.Length != 1 || args[0] != "acp") Environment.Exit(42);
    string pending = null;
    string line;
    while ((line = Console.ReadLine()) != null) {
      string id = Regex.Match(line, "\"id\"\\s*:\\s*([0-9]+)").Groups[1].Value;
      string method = Regex.Match(line, "\"method\"\\s*:\\s*\"([^\"]+)\"").Groups[1].Value;
      if (method == "initialize") {
        if (Environment.GetEnvironmentVariable("FAKE_ACP_EXIT") == "1") Environment.Exit(17);
        Reply(id, "{\"protocolVersion\":1,\"agentCapabilities\":{}}");
      } else if (method == "session/new" || method == "session/load") {
        Reply(id, "{\"sessionId\":\"fake-" + Process.GetCurrentProcess().Id + "-console-" + GetConsoleWindow().ToInt64() + "\"}");
      } else if (method == "session/prompt") {
        if (Environment.GetEnvironmentVariable("FAKE_ACP_EXIT") == "turn") Environment.Exit(18);
        if (Environment.GetEnvironmentVariable("FAKE_ACP_EXIT") == "final") {
          Reply(id, "{\"stopReason\":\"end_turn\"}");
          Console.Out.Flush();
          Environment.Exit(0);
        }
        pending = id;
        Console.WriteLine("{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"fake-" + Process.GetCurrentProcess().Id + "-console-" + GetConsoleWindow().ToInt64() + "\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"local fixture ready\"}}}}");
      } else if (method == "session/cancel" && pending != null) {
        Reply(pending, "{\"stopReason\":\"cancelled\"}"); pending = null;
      } else if (id.Length > 0) Reply(id, "{}");
    }
  }
}
