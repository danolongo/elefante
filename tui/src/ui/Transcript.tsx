import { flatSegments, type SessionState } from "../session";
import type { Tool } from "../protocol";
import { MARKDOWN_STYLE } from "./markdown";
import { toolView } from "./toolView";

function ToolCall({ tool }: { tool: Tool }) {
  const glyph = { running: "◌", ok: "✓", error: "✗", cancelled: "⊘" };
  const { subject, body, failed } = toolView(tool);
  return <box flexDirection="column">
    <text>{glyph[tool.state]} {tool.name}{subject && ` ${subject}`}{tool.duration_ms === undefined ? "" : ` (${tool.duration_ms} ms)`}</text>
    {body.length > 0 && <box flexDirection="column" paddingLeft={4}>
      {body.map((line, i) => <text key={i} fg={failed ? "#ff7777" : "#999999"}>{line}</text>)}
    </box>}
  </box>;
}
/**
 * A turn's output is drawn in the order it happened, so a tool call sits between the text before
 * and after it. Separate renderables, shading and indent preserve the channel distinction at the
 * render site. Snapshots carry no ordering, so a turn restored from one has no `segments` and is
 * drawn reasoning, then text, then its tools.
 */
export function Transcript({ state, showReasoning }: { state: SessionState; showReasoning: boolean }) {
  const turns = [...state.turns, ...(state.active_turn ? [state.active_turn] : [])];
  // flexBasis 0: the content's own height is the whole conversation, and as the flex basis it would
  // shrink the status and input rows below until the transcript painted over them.
  return <scrollbox flexGrow={1} flexBasis={0} minHeight={0} stickyScroll stickyStart="bottom" scrollX={false} contentOptions={{ flexDirection: "column", minHeight: 0 }}>
    {turns.map(turn => {
      const live = turn.turn_id === state.active_turn?.turn_id;
      const shown = (turn.segments ?? flatSegments(turn.reasoning ?? "", turn.text, turn.tools))
        .map((segment, index) => ({ segment, index })).filter(({ segment }) => showReasoning || segment.kind !== "reasoning");
      return <box key={turn.turn_id} id={`turn-${turn.turn_id}`} flexDirection="column" marginBottom={1}>
        {turn.prompt && <box flexDirection="column" backgroundColor="#1c2433" paddingX={1} marginBottom={1}><text>{turn.prompt}</text></box>}
        {shown.map(({ segment, index }, position) => {
          const next = shown[position + 1]?.segment;
          // A blank line after each block and after a run of tool calls; parallel calls stay together.
          const gap = next === undefined || segment.kind === "tool" && next.kind === "tool" ? 0 : 1;
          if (segment.kind === "reasoning") return <box key={index} id={`reasoning-${turn.turn_id}-${index}`} flexDirection="column" paddingLeft={2} marginBottom={gap}>
            <text fg="#777777">{segment.text}</text>
          </box>;
          if (segment.kind === "text") return <box key={index} id={`answer-${turn.turn_id}-${index}`} flexDirection="column" marginBottom={gap}>
            <markdown content={segment.text} syntaxStyle={MARKDOWN_STYLE} streaming={live && next === undefined} />
          </box>;
          const tool = turn.tools.find(t => t.tool_call_id === segment.tool_call_id);
          return tool && <box key={index} flexDirection="column" marginBottom={gap}><ToolCall tool={tool} /></box>;
        })}
        {(turn.memory_note_ids?.length ?? 0) > 0 && <text fg="#777777">◇ used {turn.memory_note_ids!.length} saved {turn.memory_note_ids!.length === 1 ? "note" : "notes"}</text>}
      </box>;
    })}
  </scrollbox>;
}
