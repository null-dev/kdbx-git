<script lang="ts">
	import { cn } from '$lib/utils';

	let { class: className = '', children, ...restProps } = $props<{
		class?: string;
		for?: string;
		children?: import('svelte').Snippet;
	}>();
</script>

<label class={cn('text-sm font-medium text-foreground', className)} {...restProps}>
	{@render children?.()}
</label>
