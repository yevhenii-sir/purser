import { createContext } from "react"

/** The id a `Field` gives its control: `Input` and `Textarea` take it, so the field's label names
 * the control (screen readers, a click on the label focuses it). */
export const FieldIdContext = createContext<string | undefined>(undefined)
