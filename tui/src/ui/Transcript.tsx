import type { SessionState } from "../session";
import type { Tool } from "../protocol";

function Tools({ tools }: { tools: Tool[] }) {
  const glyph = { running: "◌", ok: "✓", error: "✗", cancelled: "⊘" };
  return <box flexDirection="column">{tools.map(tool => <box key={tool.tool_call_id} flexDirection="column">
    <text>{glyph[tool.state]} {tool.name}{tool.duration_ms === undefined ? "" : ` (${tool.duration_ms} ms)`}</text>
    {tool.input_preview && <text fg="#999999">{tool.input_preview}</text>}
    {tool.output_preview && <text fg={tool.is_error ? "#ff7777" : "#999999"}>{tool.output_preview}</text>}
  </box>)}</box>;
}
/** Separate renderables and headings preserve the channel distinction at the render site. */
export function Transcript({ state, showReasoning = true }: { state: SessionState; showReasoning?: boolean }) {
  const turns = [...state.turns, ...(state.active_turn ? [state.active_turn] : [])];
  return <scrollbox flexGrow={1} stickyScroll stickyStart="bottom" scrollX={false} contentOptions={{ flexDirection: "column", minHeight: 0 }}>
    {turns.map(turn => <box key={turn.turn_id} id={`turn-${turn.turn_id}`} flexDirection="column" marginBottom={1}>
      {turn.prompt && <box flexDirection="column"><text fg="#88bbff">You</text><text>{turn.prompt}</text></box>}
      {turn.reasoning && <box id={`reasoning-${turn.turn_id}`} flexDirection="column">
        <text fg="#777777">Reasoning{showReasoning ? "" : " (collapsed)"}</text>
        {showReasoning && <text fg="#777777">{turn.reasoning}</text>}
      </box>}
      {turn.text && <box id={`answer-${turn.turn_id}`} flexDirection="column"><text fg="#88ccaa">Answer</text><text>{turn.text}</text></box>}
      <Tools tools={turn.tools} />
    </box>)}
  </scrollbox>;
}
