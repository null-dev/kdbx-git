<script lang="ts">
	import { cva, type VariantProps } from 'class-variance-authority';
	import type { HTMLButtonAttributes } from 'svelte/elements';

	import { cn } from '$lib/utils';

	const buttonVariants = cva(
		'inline-flex items-center justify-center gap-2 rounded-lg text-sm font-medium whitespace-nowrap transition-all outline-none disabled:pointer-events-none disabled:opacity-50 focus-visible:ring-2 focus-visible:ring-ring/60 focus-visible:ring-offset-2 focus-visible:ring-offset-background',
		{
			variants: {
				variant: {
					default: 'bg-primary text-primary-foreground shadow-sm hover:brightness-95',
					secondary: 'bg-secondary text-secondary-foreground hover:bg-accent',
					ghost: 'text-muted-foreground hover:bg-secondary hover:text-foreground',
					outline: 'border border-border bg-card text-card-foreground hover:bg-secondary'
				},
				size: {
					default: 'h-10 px-4 py-2',
					sm: 'h-9 rounded-md px-3 text-xs',
					lg: 'h-11 rounded-xl px-5',
					icon: 'h-10 w-10'
				}
			},
			defaultVariants: {
				variant: 'default',
				size: 'default'
			}
		}
	);

	type Props = HTMLButtonAttributes & VariantProps<typeof buttonVariants>;

	let {
		class: className = '',
		variant = 'default',
		size = 'default',
		children,
		...restProps
	}: Props = $props();
</script>

<button class={cn(buttonVariants({ variant, size }), className)} {...restProps}>
	{@render children?.()}
</button>
