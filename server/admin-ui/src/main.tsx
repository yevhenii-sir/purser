import { StrictMode } from "react"
import { createRoot } from "react-dom/client"
import { ThemeProvider } from "next-themes"
import "./index.css"
import App from "./App.tsx"

// The demo build sets the clock back to when its sample data was taken, before anything reads it.
if (__DEMO__) (await import("./lib/demo")).installClock()

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ThemeProvider attribute="class" defaultTheme="system" enableSystem disableTransitionOnChange storageKey="purserAdminTheme">
      <App />
    </ThemeProvider>
  </StrictMode>,
)
