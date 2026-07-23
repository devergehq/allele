# Accessibility Acceptance Requirements

These checks apply to every primary Allele workflow and every new surface.

## Keyboard

- Every primary pointer action has a keyboard path.
- Visible focus identifies the active control.
- Modal focus remains inside the modal until dismissal.
- Dismissing a modal restores focus to its originating control.
- Destructive confirmations default to the neutral or cancel action.

## Interaction

- Dense primary controls provide at least a 24-point interaction height.
- Important actions remain visible without hover.
- Disabled actions explain their unmet prerequisite.
- Status uses text or shape in addition to color.

## System Appearance

- Reduce Motion removes nonessential repeating and entrance animation.
- Increase Contrast preserves text, focus, and control boundaries.
- Reduce Transparency preserves surface separation and legibility.

## Announcements

- Pending, successful, and failed operations expose textual status.
- Permission requests and questions remain distinct from routine output.
- Errors remain attached to their originating action until resolved.

## Release Check

Test the primary session lifecycle, composer, Changes panel, settings, and modal flows using keyboard-only navigation. Repeat with Reduce Motion, Increase Contrast, and Reduce Transparency enabled in macOS System Settings.
