-- Connects or disconnects the Simulator's external display through its
-- I/O > External Displays menu (simctl has no command for this).
--   osascript scripts/sim_external_display.applescript connect|disconnect
on run argv
	set action to item 1 of argv
	tell application "Simulator" to activate
	delay 1
	tell application "System Events"
		tell process "Simulator"
			set displaysMenu to menu 1 of menu item "External Displays" of menu 1 of menu bar item "I/O" of menu bar 1
			set itemNames to name of every menu item of displaysMenu
			repeat with itemName in itemNames
				set itemText to itemName as text
				if action is "disconnect" and itemText contains "Disabled" then
					click menu item itemText of displaysMenu
					return "clicked " & itemText
				else if action is "connect" and (itemText contains "1920" or itemText contains "1080") then
					click menu item itemText of displaysMenu
					return "clicked " & itemText
				end if
			end repeat
			return "no matching item in: " & (itemNames as text)
		end tell
	end tell
end run
