import { useEffect, useState } from "react";
import { commands, events, type SnapshotEvent_Serialize } from "./bindings";

// Thin view (AD-5): it sends commands and shows the last snapshot, nothing more.
function App() {
  const [snapshot, setSnapshot] = useState<SnapshotEvent_Serialize | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const unlisten = events.snapshotEvent.listen((e) => setSnapshot(e.payload));
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  async function startDemo() {
    const result = await commands.startDemo();
    setError(result.status === "error" ? result.error : null);
  }

  return (
    <main>
      <h1>nguda</h1>
      <button type="button" onClick={startDemo}>
        Start demo
      </button>
      <p>
        {snapshot
          ? `rev ${snapshot.rev} · ${snapshot.running?.title ?? "idle"}`
          : "No snapshot yet"}
      </p>
      {error && <p role="alert">{error}</p>}
    </main>
  );
}

export default App;
