import { useEffect, useState } from "react";
import { events, type SnapshotEvent_Serialize } from "./bindings";

// Thin view (AD-5): it shows the last snapshot, nothing more. The plan view and its
// commands arrive with the window story.
function App() {
  const [snapshot, setSnapshot] = useState<SnapshotEvent_Serialize | null>(null);

  useEffect(() => {
    const unlisten = events.snapshotEvent.listen((e) => setSnapshot(e.payload));
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  return (
    <main>
      <h1>nguda</h1>
      <p>
        {snapshot
          ? `rev ${snapshot.rev} · ${snapshot.running?.title ?? "idle"}`
          : "No snapshot yet"}
      </p>
    </main>
  );
}

export default App;
