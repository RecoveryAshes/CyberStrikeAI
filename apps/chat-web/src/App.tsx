import { TooltipProvider } from "./components/ui/tooltip";
import { ChatWorkbench } from "./components/workbench/ChatWorkbench";

export default function App() {
  return (
    <TooltipProvider delayDuration={150}>
      <ChatWorkbench />
    </TooltipProvider>
  );
}
