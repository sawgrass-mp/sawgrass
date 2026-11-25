--!strict
local online: { [number]: boolean } = {}
local count = 0

sawgrass.on("playerJoin", function(player: number)
	online[player] = true
	count += 1
	print("player joined", player, "online", count)
end)

sawgrass.on("playerLeave", function(player: number)
	if online[player] then
		online[player] = nil
		count -= 1
	end
	print("player left", player, "online", count)
end)

sawgrass.on("tick", function(tick: number, dt: number)
	if tick % 600 == 0 then
		print("tick", tick, "online", count)
	end
end)
