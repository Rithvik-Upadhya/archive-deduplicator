<!--
    Bundled icon renderer.

    Icons MUST go through this component. It maps a Phosphor `ph:*` name onto a
    `phosphor-svelte` component that is compiled into the bundle. Do not import
    `@iconify/svelte` -- it carries no icon data and fetches every glyph from
    api.iconify.design at runtime, so all icons silently vanish offline, which is
    the one condition this app exists for.

    Adding an icon means adding its base name to `BASE` below; `IconName` is
    derived from that map, so `pnpm check` fails on any name that is used but not
    bundled.

    The few glyphs Phosphor lacks live in `./icons/` as inline-SVG components
    and are named `app:*` via `CUSTOM` below -- same compile-time check.
-->
<script lang="ts" module>
    import type { Component } from 'svelte';
    import ArrowCounterClockwise from 'phosphor-svelte/lib/ArrowCounterClockwise';
    import ArrowsOutCardinal from 'phosphor-svelte/lib/ArrowsOutCardinal';
    import CaretRight from 'phosphor-svelte/lib/CaretRight';
    import CaretUpDown from 'phosphor-svelte/lib/CaretUpDown';
    import Check from 'phosphor-svelte/lib/Check';
    import CheckCircle from 'phosphor-svelte/lib/CheckCircle';
    import CircleDashed from 'phosphor-svelte/lib/CircleDashed';
    import Copy from 'phosphor-svelte/lib/Copy';
    import CopySimple from 'phosphor-svelte/lib/CopySimple';
    import DotsThreeOutline from 'phosphor-svelte/lib/DotsThreeOutline';
    import Export from 'phosphor-svelte/lib/Export';
    import Eye from 'phosphor-svelte/lib/Eye';
    import EyeSlash from 'phosphor-svelte/lib/EyeSlash';
    import File from 'phosphor-svelte/lib/File';
    import FilePlus from 'phosphor-svelte/lib/FilePlus';
    import Folder from 'phosphor-svelte/lib/Folder';
    import FolderOpen from 'phosphor-svelte/lib/FolderOpen';
    import FolderPlus from 'phosphor-svelte/lib/FolderPlus';
    import Funnel from 'phosphor-svelte/lib/Funnel';
    import GitPullRequest from 'phosphor-svelte/lib/GitPullRequest';
    import HardDrive from 'phosphor-svelte/lib/HardDrive';
    import HardDrives from 'phosphor-svelte/lib/HardDrives';
    import MagnifyingGlass from 'phosphor-svelte/lib/MagnifyingGlass';
    import Moon from 'phosphor-svelte/lib/Moon';
    import Password from 'phosphor-svelte/lib/Password';
    import PencilSimple from 'phosphor-svelte/lib/PencilSimple';
    import PlusCircle from 'phosphor-svelte/lib/PlusCircle';
    import Scan from 'phosphor-svelte/lib/Scan';
    import SidebarSimple from 'phosphor-svelte/lib/SidebarSimple';
    import SpinnerGap from 'phosphor-svelte/lib/SpinnerGap';
    import Stack from 'phosphor-svelte/lib/Stack';
    import Sun from 'phosphor-svelte/lib/Sun';
    import Trash from 'phosphor-svelte/lib/Trash';
    import Textbox from 'phosphor-svelte/lib/Textbox';
    import TextStrikethrough from 'phosphor-svelte/lib/TextStrikethrough';
    import TreeStructure from 'phosphor-svelte/lib/TreeStructure';
    import TreeView from 'phosphor-svelte/lib/TreeView';
    import Warning from 'phosphor-svelte/lib/Warning';
    import WarningCircle from 'phosphor-svelte/lib/WarningCircle';
    import X from 'phosphor-svelte/lib/X';
    import XCircle from 'phosphor-svelte/lib/XCircle';
    import FileCheck from './icons/FileCheck.svelte';
    import FolderCheck from './icons/FolderCheck.svelte';

    /** Phosphor base name (the icon name minus its weight suffix) -> component. */
    const BASE = {
        'arrow-counter-clockwise': ArrowCounterClockwise,
        'arrows-out-cardinal': ArrowsOutCardinal,
        'caret-right': CaretRight,
        'caret-up-down': CaretUpDown,
        check: Check,
        'check-circle': CheckCircle,
        'circle-dashed': CircleDashed,
        copy: Copy,
        'copy-simple': CopySimple,
        'dots-three-outline': DotsThreeOutline,
        export: Export,
        eye: Eye,
        'eye-slash': EyeSlash,
        file: File,
        'file-plus': FilePlus,
        folder: Folder,
        'folder-open': FolderOpen,
        'folder-plus': FolderPlus,
        funnel: Funnel,
        'git-pull-request': GitPullRequest,
        'hard-drive': HardDrive,
        'hard-drives': HardDrives,
        'magnifying-glass': MagnifyingGlass,
        moon: Moon,
        password: Password,
        'pencil-simple': PencilSimple,
        'plus-circle': PlusCircle,
        scan: Scan,
        'sidebar-simple': SidebarSimple,
        'spinner-gap': SpinnerGap,
        stack: Stack,
        sun: Sun,
        textbox: Textbox,
        'text-strikethrough': TextStrikethrough,
        trash: Trash,
        'tree-structure': TreeStructure,
        'tree-view': TreeView,
        warning: Warning,
        'warning-circle': WarningCircle,
        x: X,
        'x-circle': XCircle,
    };

    type BaseName = keyof typeof BASE;

    /** Non-Phosphor glyphs (no weights), rendered with `class` only. */
    const CUSTOM = {
        'app:file-check': FileCheck,
        'app:folder-check': FolderCheck,
    };

    type CustomName = keyof typeof CUSTOM;

    const WEIGHTS = ['thin', 'light', 'bold', 'fill', 'duotone'] as const;
    type Weight = (typeof WEIGHTS)[number];

    /**
     * Every icon name this app may use. Derived from `BASE`, so a name whose base
     * is not bundled is a type error rather than an icon that renders nothing.
     */
    export type IconName =
        | `ph:${BaseName}`
        | `ph:${BaseName}-${Weight}`
        | CustomName;

    /**
     * The phosphor components all share one prop shape, but `BASE`'s inferred
     * value type is a 37-member union of distinct component types, which is
     * awkward to render dynamically. Collapse it to the shape they have in
     * common at the point of use.
     */
    type IconComp = Component<{ weight?: string; class?: string }>;
    const comp = (k: BaseName) => BASE[k] as unknown as IconComp;

    function resolve(
        name: IconName
    ): { Comp: IconComp; weight: string } | null {
        if (name in CUSTOM) {
            return {
                Comp: CUSTOM[name as CustomName] as unknown as IconComp,
                weight: 'regular',
            };
        }
        const raw: string = name.slice('ph:'.length);
        if (raw in BASE) {
            return { Comp: comp(raw as BaseName), weight: 'regular' };
        }
        const cut = raw.lastIndexOf('-');
        if (cut < 0) return null;
        const base = raw.slice(0, cut);
        const weight = raw.slice(cut + 1);
        if ((WEIGHTS as readonly string[]).includes(weight) && base in BASE) {
            return { Comp: comp(base as BaseName), weight };
        }
        return null;
    }
</script>

<script lang="ts">
    interface Props {
        icon: IconName;
        class?: string;
    }

    let { icon, class: className }: Props = $props();

    const resolved = $derived(resolve(icon));
</script>

{#if resolved}
    {@const Comp = resolved.Comp}
    <Comp weight={resolved.weight} class={className} />
{/if}
