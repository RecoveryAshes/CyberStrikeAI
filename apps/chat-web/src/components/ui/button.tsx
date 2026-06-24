import * as React from "react"
import { Slot } from "@radix-ui/react-slot"
import { cva, type VariantProps } from "class-variance-authority"

import { cn } from "@/lib/utils"

const buttonVariants = cva(
  "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-full text-sm font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/30 disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        default: "bg-white/[.90] text-zinc-950 hover:bg-white",
        destructive:
          "bg-red-500/[.85] text-white hover:bg-red-500",
        outline:
          "border border-white/[.12] bg-transparent text-zinc-100 hover:bg-white/10",
        secondary:
          "bg-white/10 text-zinc-100 hover:bg-white/[.14]",
        ghost: "text-zinc-200 hover:bg-white/10 hover:text-white",
        link: "text-zinc-200 underline-offset-4 hover:underline",
        glass: "border border-white/[.12] bg-white/[.08] text-zinc-100 shadow-glass hover:bg-white/[.12]",
        subtle: "bg-white/6 text-zinc-200 hover:bg-white/10",
        danger: "bg-red-500/[.85] text-white hover:bg-red-500",
      },
      size: {
        default: "h-9 px-4",
        sm: "h-8 px-3 text-xs",
        lg: "h-11 px-8",
        icon: "h-9 w-9",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  }
)

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean
}

const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, asChild = false, ...props }, ref) => {
    const Comp = asChild ? Slot : "button"
    return (
      <Comp
        className={cn(buttonVariants({ variant, size, className }))}
        ref={ref}
        {...props}
      />
    )
  }
)
Button.displayName = "Button"

export { Button, buttonVariants }
